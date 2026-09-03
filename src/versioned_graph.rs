//! Exact-version dependency graph cycle detection and finite symlink materialization.
//!
//! The ordinary one-version solver remains useful when every requirement can share
//! one package version. This module owns the stronger graph shape required when
//! separate parents intentionally select separate versions or when an exact
//! package-version edge closes a cycle. Package payloads remain immutable and are
//! never recursively mirrored into descendant directories.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail, ensure};
use next_loggers::{JsonObject, Logger, Options, json};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zed_interfaces::dependency_graph::{
    DependencyGraphCompleteness, DependencyGraphData, DependencyGraphDocument,
    PackageVersionIdentity, ResolvedDependencyEdge, ResolvedDependencyNode,
};

use crate::store::Store;

pub const LOCAL_MATERIALIZATION_SCHEMA_V1: &str = "zpkg/local-graph-materialization/v1";
pub const CYCLE_EVENT: &str = "zed.install.circular_dependency";
pub const RESOLUTION_MODE: &str = "exact-node-symlink";
pub const MATERIALIZATION_REPORT_SCHEMA_V1: &str = "zpkg/versioned-graph-materialization-report/v1";
const MAX_MATERIALIZED_NODES: usize = 50_000;
const MAX_MATERIALIZED_EDGES: usize = 500_000;
const MAX_CYCLE_WITNESSES: usize = 4_096;

fn local_materialization_schema_v1() -> String {
    LOCAL_MATERIALIZATION_SCHEMA_V1.to_string()
}

/// Local payload location for one exact node in the canonical resolved graph.
///
/// `source` is interpreted relative to the plan file's parent when it is not
/// absolute. Production callers should point it at the immutable global store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPackageSource {
    pub id: PackageVersionIdentity,
    pub source: PathBuf,
}

/// A canonical exact resolved graph plus the local immutable source directory
/// for every node. Edges and roots remain authoritative in `graph`; filesystem
/// layout is only a projection of that document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalGraphMaterializationPlan {
    #[serde(default = "local_materialization_schema_v1")]
    pub schema: String,
    pub graph: DependencyGraphDocument,
    pub sources: Vec<LocalPackageSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionedMaterializationMode {
    Symlink,
    Copy,
}

impl FromStr for VersionedMaterializationMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "symlink" | "link" => Ok(Self::Symlink),
            "copy" | "isolate" => Ok(Self::Copy),
            other => bail!(
                "unsupported exact-graph materialization mode `{other}`; expected `symlink` or `copy`"
            ),
        }
    }
}

/// Exact materialized node identity. The graph contract keeps the artifact
/// digest on the node rather than inside [`PackageVersionIdentity`], so cycle
/// diagnostics carry both pieces explicitly.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExactDependencyNodeIdentity {
    pub package: PackageVersionIdentity,
    pub artifact_digest: String,
}

impl ExactDependencyNodeIdentity {
    pub fn label(&self) -> String {
        let digest = self
            .artifact_digest
            .strip_prefix("sha256:")
            .unwrap_or(&self.artifact_digest);
        let short = &digest[..digest.len().min(12)];
        format!("{}#{short}", self.package)
    }
}

/// One deterministic directed-cycle witness. `path` is closed: its final item
/// equals its first item. A repeated package name at another version is not a
/// cycle unless the exact package-version-and-digest identity is revisited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyCycle {
    pub cycle_id: String,
    pub path: Vec<ExactDependencyNodeIdentity>,
    pub closing_from: ExactDependencyNodeIdentity,
    pub closing_to: ExactDependencyNodeIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedMaterializationReport {
    pub schema: String,
    pub graph_digest: String,
    pub materialization_digest: String,
    pub generation_dir: PathBuf,
    pub modules_dir: PathBuf,
    pub nodes: usize,
    pub edges: usize,
    pub cycles: Vec<DependencyCycle>,
    pub reused_generation: bool,
}

#[derive(Debug, Clone)]
struct ValidatedNode {
    id: PackageVersionIdentity,
    artifact_digest: String,
    source: PathBuf,
    key: String,
}

#[derive(Debug, Clone)]
struct ValidatedPlan {
    graph: DependencyGraphDocument,
    graph_digest: String,
    materialization_digest: String,
    roots: Vec<PackageVersionIdentity>,
    nodes: BTreeMap<PackageVersionIdentity, ValidatedNode>,
    edges: Vec<ResolvedDependencyEdge>,
    cycles: Vec<DependencyCycle>,
    has_parallel_versions: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GenerationMarker {
    schema: String,
    graph_digest: String,
    materialization_digest: String,
    nodes: usize,
    edges: usize,
}

impl GenerationMarker {
    fn for_plan(plan: &ValidatedPlan) -> Self {
        Self {
            schema: MATERIALIZATION_REPORT_SCHEMA_V1.into(),
            graph_digest: plan.graph_digest.clone(),
            materialization_digest: plan.materialization_digest.clone(),
            nodes: plan.nodes.len(),
            edges: plan.edges.len(),
        }
    }
}

/// ORE structured logger used by the public CLI boundary. Console output is
/// enabled so users see exact circular-dependency paths during materialization.
pub fn terminal_cycle_logger() -> Logger {
    let fields = JsonObject::from_iter([
        ("component".into(), json!("versioned_dependency_graph")),
        ("event_family".into(), json!("dependency_resolution")),
    ]);
    Logger::new(Options {
        app_name: "zed-cli".into(),
        name: Some("exact-version-graph".into()),
        fields,
        console: true,
        ..Options::default()
    })
}

/// Read a materialization plan and project its exact graph into the project.
pub fn materialize_plan_file(
    plan_path: &Path,
    project: &Path,
    mode: VersionedMaterializationMode,
    logger: &Logger,
) -> Result<VersionedMaterializationReport> {
    let bytes = fs::read(plan_path).with_context(|| {
        format!(
            "failed to read exact dependency graph plan {}",
            plan_path.display()
        )
    })?;
    let plan: LocalGraphMaterializationPlan =
        serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "failed to parse exact dependency graph plan {}",
                plan_path.display()
            )
        })?;
    let source_base = plan_path.parent().unwrap_or_else(|| Path::new("."));
    materialize_plan(&plan, source_base, project, mode, logger)
}

/// Read one byte-canonical, digest-verified resolved graph and bind every node
/// to its immutable package directory in the configured Zed store.
pub fn materialize_graph_file_from_store(
    graph_path: &Path,
    store: &Store,
    project: &Path,
    mode: VersionedMaterializationMode,
    logger: &Logger,
) -> Result<VersionedMaterializationReport> {
    let bytes = fs::read(graph_path).with_context(|| {
        format!(
            "failed to read exact dependency graph {}",
            graph_path.display()
        )
    })?;
    let graph = DependencyGraphDocument::parse_verified_canonical(&bytes).with_context(|| {
        format!(
            "{} is not a canonical digest-verified exact dependency graph",
            graph_path.display()
        )
    })?;
    let plan = plan_from_store(graph, store)?;
    materialize_plan(&plan, Path::new("."), project, mode, logger)
}

/// Bind an exact resolved graph to the content-addressed Zed store. Missing
/// artifacts are reported explicitly; this projection never fetches or guesses.
pub fn plan_from_store(
    graph: DependencyGraphDocument,
    store: &Store,
) -> Result<LocalGraphMaterializationPlan> {
    let DependencyGraphData::Resolved { nodes, .. } = &graph.graph else {
        bail!("store materialization requires a resolved exact dependency graph");
    };
    let mut sources = Vec::with_capacity(nodes.len());
    for node in nodes {
        let digest = required_artifact_digest(node)?;
        let sha256 = digest
            .strip_prefix("sha256:")
            .context("validated artifact digest lost its sha256 prefix")?;
        let source = store.pkg_dir(sha256);
        ensure!(
            source.is_dir(),
            "immutable store artifact `{}` for exact node `{}` is missing at {}; fetch the locked graph before materializing it",
            digest,
            node.id,
            source.display()
        );
        sources.push(LocalPackageSource {
            id: node.id.clone(),
            source,
        });
    }
    Ok(LocalGraphMaterializationPlan {
        schema: LOCAL_MATERIALIZATION_SCHEMA_V1.into(),
        graph,
        sources,
    })
}

/// Validate, report, and materialize a finite exact-version dependency graph.
pub fn materialize_plan(
    plan: &LocalGraphMaterializationPlan,
    source_base: &Path,
    project: &Path,
    mode: VersionedMaterializationMode,
    logger: &Logger,
) -> Result<VersionedMaterializationReport> {
    fs::create_dir_all(project)
        .with_context(|| format!("failed to create project directory {}", project.display()))?;
    let project = project
        .canonicalize()
        .with_context(|| format!("failed to resolve project directory {}", project.display()))?;
    let validated = validate_plan(plan, source_base, &project)?;
    log_cycles(logger, &validated)?;

    if mode == VersionedMaterializationMode::Copy {
        if !validated.cycles.is_empty() {
            bail!(
                "copy mode cannot represent a circular exact-version dependency graph without recursively duplicating nodes; use `--mode symlink`"
            );
        }
        if validated.has_parallel_versions {
            bail!(
                "copy mode cannot represent multiple exact versions of one package without duplicating payloads; use `--mode symlink`"
            );
        }
        bail!(
            "copy mode is not defined for exact-version node overlays; use the ordinary flat installer for an acyclic one-version graph"
        );
    }

    #[cfg(not(unix))]
    {
        let _ = project;
        bail!(
            "exact-version graph materialization requires directory symlinks; this platform must use a symlink-capable environment"
        );
    }

    #[cfg(unix)]
    {
        materialize_symlink_graph(validated, &project)
    }
}

fn validate_plan(
    plan: &LocalGraphMaterializationPlan,
    source_base: &Path,
    project: &Path,
) -> Result<ValidatedPlan> {
    ensure!(
        plan.schema == LOCAL_MATERIALIZATION_SCHEMA_V1,
        "unsupported local graph materialization schema `{}`; expected `{LOCAL_MATERIALIZATION_SCHEMA_V1}`",
        plan.schema
    );

    let mut graph = plan.graph.clone();
    if graph.graph_digest.is_some() {
        graph
            .verify_digest()
            .context("exact dependency graph digest verification failed")?;
        graph.normalize_in_place();
    } else {
        graph = graph
            .finalize()
            .context("failed to finalize exact dependency graph")?;
    }
    let graph_digest = graph
        .graph_digest
        .clone()
        .context("finalized exact dependency graph omitted graph_digest")?;

    let (completeness, roots, graph_nodes, edges) = match &graph.graph {
        DependencyGraphData::Resolved {
            completeness,
            roots,
            nodes,
            edges,
            ..
        } => (*completeness, roots.clone(), nodes.clone(), edges.clone()),
        DependencyGraphData::Declared { .. } => {
            bail!("local graph materialization requires a resolved exact dependency graph")
        }
    };
    ensure!(
        completeness == DependencyGraphCompleteness::Complete,
        "projected dependency graphs are not lock authority and cannot be materialized"
    );
    ensure!(
        graph_nodes.len() <= MAX_MATERIALIZED_NODES,
        "exact dependency graph contains {} nodes; limit is {MAX_MATERIALIZED_NODES}",
        graph_nodes.len()
    );
    ensure!(
        edges.len() <= MAX_MATERIALIZED_EDGES,
        "exact dependency graph contains {} edges; limit is {MAX_MATERIALIZED_EDGES}",
        edges.len()
    );

    let mut source_map = BTreeMap::new();
    for source in &plan.sources {
        ensure!(
            source_map
                .insert(source.id.clone(), source.source.clone())
                .is_none(),
            "duplicate local source mapping for `{}`",
            source.id
        );
    }

    let source_base = source_base
        .canonicalize()
        .with_context(|| format!("failed to resolve source base {}", source_base.display()))?;
    let mut nodes = BTreeMap::new();
    let mut coordinate_versions: BTreeMap<(String, String, String), BTreeSet<(String, String)>> =
        BTreeMap::new();
    for node in graph_nodes {
        let artifact_digest = required_artifact_digest(&node)?;
        let source = source_map
            .remove(&node.id)
            .with_context(|| format!("missing local source mapping for `{}`", node.id))?;
        let source = if source.is_absolute() {
            source
        } else {
            source_base.join(source)
        };
        let source = source
            .canonicalize()
            .with_context(|| format!("failed to resolve package source {}", source.display()))?;
        ensure!(
            path_is_real_directory(&source)?,
            "package source is not a real directory: {}",
            source.display()
        );
        ensure!(
            !paths_overlap(&source, project),
            "package source {} overlaps project {}; immutable payloads and generated dependency links must remain disjoint",
            source.display(),
            project.display()
        );
        ensure_absent(
            &source.join("zed_modules"),
            &format!(
                "package source `{}` already contains reserved `zed_modules`; immutable payloads must not embed a project materialization tree",
                node.id
            ),
        )?;

        let key = node_key(&node.id, &artifact_digest);
        coordinate_versions
            .entry((
                node.id.registry_id.clone(),
                node.id.org.clone(),
                node.id.name.clone(),
            ))
            .or_default()
            .insert((node.id.version.clone(), artifact_digest.clone()));
        let validated = ValidatedNode {
            id: node.id.clone(),
            artifact_digest,
            source,
            key,
        };
        ensure!(
            nodes.insert(node.id.clone(), validated).is_none(),
            "duplicate exact dependency node `{}`",
            node.id
        );
    }
    ensure!(
        source_map.is_empty(),
        "local source map contains {} entry(s) absent from the exact dependency graph",
        source_map.len()
    );

    let mut root_aliases = BTreeSet::new();
    for root in &roots {
        ensure!(
            nodes.contains_key(root),
            "root node `{root}` is not materializable"
        );
        ensure!(
            root_aliases.insert((root.org.clone(), root.name.clone())),
            "root aliases collide at `zed_modules/{}/{}`; select one direct root version per package coordinate",
            root.org,
            root.name
        );
    }

    let mut dependency_aliases = BTreeSet::new();
    for edge in &edges {
        ensure!(
            nodes.contains_key(&edge.from),
            "edge source `{}` is not materializable",
            edge.from
        );
        ensure!(
            nodes.contains_key(&edge.to),
            "edge target `{}` is not materializable",
            edge.to
        );
        ensure!(
            dependency_aliases.insert((
                edge.from.clone(),
                edge.to.org.clone(),
                edge.to.name.clone(),
            )),
            "package `{}` has multiple exact targets for dependency alias `zed_modules/{}/{}`",
            edge.from,
            edge.to.org,
            edge.to.name
        );
    }

    let cycle_paths = detect_cycle_paths(&roots, nodes.keys(), &edges)?;
    let cycles = decorate_cycles(cycle_paths, &nodes)?;
    let has_parallel_versions = coordinate_versions
        .values()
        .any(|versions| versions.len() > 1);
    let materialization_digest = compute_materialization_digest(&graph_digest, &nodes)?;
    Ok(ValidatedPlan {
        graph,
        graph_digest,
        materialization_digest,
        roots,
        nodes,
        edges,
        cycles,
        has_parallel_versions,
    })
}

fn required_artifact_digest(node: &ResolvedDependencyNode) -> Result<String> {
    let digest = node
        .artifact_digest
        .clone()
        .with_context(|| format!("exact dependency node `{}` has no artifact_digest", node.id))?;
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!(
            "exact dependency node `{}` artifact_digest must use `sha256:<lowercase-hex>`",
            node.id
        );
    };
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "exact dependency node `{}` has a non-canonical artifact_digest",
        node.id
    );
    ensure!(
        hex.bytes().any(|byte| byte != b'0'),
        "exact dependency node `{}` has an all-zero artifact_digest",
        node.id
    );
    Ok(digest)
}

fn path_is_real_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect directory {}", path.display()))
        }
    }
}

fn ensure_absent(path: &Path, message: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("{message}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn compute_materialization_digest(
    graph_digest: &str,
    nodes: &BTreeMap<PackageVersionIdentity, ValidatedNode>,
) -> Result<String> {
    #[derive(Serialize)]
    struct Binding<'a> {
        package: &'a PackageVersionIdentity,
        artifact_digest: &'a str,
        source: String,
    }

    let bindings = nodes
        .values()
        .map(|node| Binding {
            package: &node.id,
            artifact_digest: &node.artifact_digest,
            source: node.source.to_string_lossy().into_owned(),
        })
        .collect::<Vec<_>>();
    let payload = serde_json::to_vec(&(graph_digest, bindings))
        .context("serializing exact graph source bindings")?;
    Ok(format!("sha256:{}", hex_sha256(&payload)))
}

fn detect_cycle_paths<'a, I>(
    roots: &[PackageVersionIdentity],
    nodes: I,
    edges: &[ResolvedDependencyEdge],
) -> Result<Vec<Vec<PackageVersionIdentity>>>
where
    I: IntoIterator<Item = &'a PackageVersionIdentity>,
{
    let mut adjacency: BTreeMap<PackageVersionIdentity, BTreeSet<PackageVersionIdentity>> =
        BTreeMap::new();
    for node in nodes {
        adjacency.entry(node.clone()).or_default();
    }
    for edge in edges {
        adjacency
            .entry(edge.from.clone())
            .or_default()
            .insert(edge.to.clone());
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Visit {
        Visiting(usize),
        Done,
    }

    #[derive(Debug)]
    struct Frame {
        node: PackageVersionIdentity,
        children: Vec<PackageVersionIdentity>,
        next_child: usize,
    }

    impl Frame {
        fn new(
            node: PackageVersionIdentity,
            adjacency: &BTreeMap<PackageVersionIdentity, BTreeSet<PackageVersionIdentity>>,
        ) -> Self {
            Self {
                children: adjacency
                    .get(&node)
                    .map(|children| children.iter().cloned().collect())
                    .unwrap_or_default(),
                node,
                next_child: 0,
            }
        }
    }

    let mut states = BTreeMap::new();
    let mut stack = Vec::new();
    let mut found: BTreeMap<String, Vec<PackageVersionIdentity>> = BTreeMap::new();
    let mut traversal = roots.to_vec();
    traversal.extend(adjacency.keys().cloned());
    traversal.sort();
    traversal.dedup();

    for start in traversal {
        if states.contains_key(&start) {
            continue;
        }
        states.insert(start.clone(), Visit::Visiting(0));
        stack.push(start.clone());
        let mut frames = vec![Frame::new(start, &adjacency)];

        while !frames.is_empty() {
            let next_child = {
                let frame = frames.last_mut().expect("checked non-empty frame stack");
                if frame.next_child == frame.children.len() {
                    None
                } else {
                    let child = frame.children[frame.next_child].clone();
                    frame.next_child += 1;
                    Some(child)
                }
            };

            let Some(child) = next_child else {
                let frame = frames.pop().expect("checked non-empty frame stack");
                let popped = stack.pop().expect("DFS node stack matches frame stack");
                debug_assert_eq!(popped, frame.node);
                states.insert(frame.node, Visit::Done);
                continue;
            };

            match states.get(&child).copied() {
                None => {
                    states.insert(child.clone(), Visit::Visiting(stack.len()));
                    stack.push(child.clone());
                    frames.push(Frame::new(child, &adjacency));
                }
                Some(Visit::Visiting(index)) => {
                    let mut path = stack[index..].to_vec();
                    path.push(child);
                    let path = canonical_cycle(path);
                    let key = path
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" -> ");
                    if !found.contains_key(&key) && found.len() >= MAX_CYCLE_WITNESSES {
                        bail!(
                            "exact dependency graph exposes more than {MAX_CYCLE_WITNESSES} deterministic cycle witnesses; refusing to flood diagnostics"
                        );
                    }
                    found.entry(key).or_insert(path);
                }
                Some(Visit::Done) => {}
            }
        }
    }
    Ok(found.into_values().collect())
}

fn canonical_cycle(path: Vec<PackageVersionIdentity>) -> Vec<PackageVersionIdentity> {
    debug_assert!(path.len() >= 2 && path.first() == path.last());
    let body = &path[..path.len() - 1];
    let mut best = 0;
    for start in 1..body.len() {
        let candidate = (0..body.len())
            .map(|offset| &body[(start + offset) % body.len()])
            .collect::<Vec<_>>();
        let current = (0..body.len())
            .map(|offset| &body[(best + offset) % body.len()])
            .collect::<Vec<_>>();
        if candidate < current {
            best = start;
        }
    }
    let mut canonical = (0..body.len())
        .map(|offset| body[(best + offset) % body.len()].clone())
        .collect::<Vec<_>>();
    canonical.push(canonical[0].clone());
    canonical
}

fn decorate_cycles(
    paths: Vec<Vec<PackageVersionIdentity>>,
    nodes: &BTreeMap<PackageVersionIdentity, ValidatedNode>,
) -> Result<Vec<DependencyCycle>> {
    paths
        .into_iter()
        .map(|path| {
            let exact_path = path
                .iter()
                .map(|id| {
                    let node = nodes
                        .get(id)
                        .with_context(|| format!("cycle references absent exact node `{id}`"))?;
                    Ok(ExactDependencyNodeIdentity {
                        package: id.clone(),
                        artifact_digest: node.artifact_digest.clone(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let key = exact_path
                .iter()
                .map(ExactDependencyNodeIdentity::label)
                .collect::<Vec<_>>()
                .join(" -> ");
            let closing_from = exact_path[exact_path.len() - 2].clone();
            let closing_to = exact_path[exact_path.len() - 1].clone();
            Ok(DependencyCycle {
                cycle_id: format!("sha256:{}", hex_sha256(key.as_bytes())),
                path: exact_path,
                closing_from,
                closing_to,
            })
        })
        .collect()
}

fn log_cycles(logger: &Logger, plan: &ValidatedPlan) -> Result<()> {
    for cycle in &plan.cycles {
        let exact_path = cycle
            .path
            .iter()
            .map(ExactDependencyNodeIdentity::label)
            .collect::<Vec<_>>();
        let closing_from = cycle.closing_from.label();
        let closing_to = cycle.closing_to.label();
        let message = format!(
            "circular dependency detected: {}; closing edge {} -> {} reuses the existing exact node through a symlink",
            exact_path.join(" -> "),
            closing_from,
            closing_to
        );
        let target = &plan.nodes[&cycle.closing_to.package];
        let fields = JsonObject::from_iter([
            ("event".into(), json!(CYCLE_EVENT)),
            ("cycle_id".into(), json!(&cycle.cycle_id)),
            ("path".into(), json!(&exact_path)),
            ("closing_from".into(), json!(closing_from)),
            ("closing_to".into(), json!(closing_to)),
            ("target_node_key".into(), json!(&target.key)),
            (
                "target_artifact_digest".into(),
                json!(&target.artifact_digest),
            ),
            ("resolution".into(), json!(RESOLUTION_MODE)),
            ("graph_digest".into(), json!(&plan.graph_digest)),
            (
                "materialization_digest".into(),
                json!(&plan.materialization_digest),
            ),
        ]);
        logger
            .warn(vec![json!(message)])
            .add_fields(fields)
            .add_tags(["zed-pkg", "dependency-cycle", RESOLUTION_MODE])
            .send()
            .context("ORE cycle log delivery failed")?;
    }
    Ok(())
}

fn node_key(id: &PackageVersionIdentity, artifact_digest: &str) -> String {
    let identity = format!(
        "{}\0{}\0{}\0{}\0{}",
        id.registry_id, id.org, id.name, id.version, artifact_digest
    );
    let identity_digest = hex_sha256(identity.as_bytes());
    format!(
        "{}+{}@{}+{}",
        id.org,
        id.name,
        sanitize_segment(&id.version),
        &identity_digest[..16]
    )
}

fn sanitize_segment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "version".into()
    } else {
        sanitized
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(unix)]
fn create_real_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "refusing non-directory or symlinked control path {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).with_context(|| {
                format!("failed to create control directory {}", path.display())
            })?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect control path {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn materialize_symlink_graph(
    plan: ValidatedPlan,
    project: &Path,
) -> Result<VersionedMaterializationReport> {
    use std::os::unix::fs::symlink;

    let dot_zed = project.join(".zed");
    create_real_directory(&dot_zed)?;
    let graph_control = dot_zed.join("versioned-graph");
    create_real_directory(&graph_control)?;
    let schema_control = graph_control.join("v1");
    create_real_directory(&schema_control)?;
    let graphs = schema_control.join("graphs");
    create_real_directory(&graphs)?;

    let graph_id = plan
        .graph_digest
        .strip_prefix("sha256:")
        .context("graph_digest must use sha256")?;
    let materialization_id = plan
        .materialization_digest
        .strip_prefix("sha256:")
        .context("materialization_digest must use sha256")?;
    let graph_group = graphs.join(graph_id);
    create_real_directory(&graph_group)?;
    let generation = graph_group.join(materialization_id);

    let reused_generation = match fs::symlink_metadata(&generation) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "refusing non-directory or symlinked graph generation {}",
                generation.display()
            );
            verify_generation(&generation, &plan)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let staging = graph_group.join(format!(".tmp-{}", Uuid::new_v4()));
            let build_result = build_generation(&staging, &generation, &plan);
            if build_result.is_err() {
                let _ = remove_path(&staging);
            }
            build_result?;
            false
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect graph generation {}",
                    generation.display()
                )
            });
        }
    };

    let modules_staging = schema_control.join(format!(".modules-{}", Uuid::new_v4()));
    create_real_directory(&modules_staging)?;
    let root_links = (|| -> Result<()> {
        for root in &plan.roots {
            let node = &plan.nodes[root];
            let alias_parent = modules_staging.join(&root.org);
            if !alias_parent.exists() {
                create_real_directory(&alias_parent)?;
            }
            symlink(
                generation.join("nodes").join(&node.key).join("root"),
                alias_parent.join(&root.name),
            )
            .with_context(|| format!("failed to link direct root `{root}`"))?;
        }
        Ok(())
    })();
    if let Err(error) = root_links {
        let _ = remove_path(&modules_staging);
        return Err(error);
    }

    let modules_dir = project.join("zed_modules");
    if let Err(error) =
        replace_directory_atomically(&modules_staging, &modules_dir, &schema_control)
    {
        let _ = remove_path(&modules_staging);
        return Err(error);
    }

    Ok(VersionedMaterializationReport {
        schema: MATERIALIZATION_REPORT_SCHEMA_V1.into(),
        graph_digest: plan.graph_digest,
        materialization_digest: plan.materialization_digest,
        generation_dir: generation,
        modules_dir,
        nodes: plan.nodes.len(),
        edges: plan.edges.len(),
        cycles: plan.cycles,
        reused_generation,
    })
}

#[cfg(unix)]
fn build_generation(staging: &Path, generation: &Path, plan: &ValidatedPlan) -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::fs::symlink;

    create_real_directory(staging)?;
    create_real_directory(&staging.join("nodes"))?;
    for node in plan.nodes.values() {
        let node_dir = staging.join("nodes").join(&node.key);
        create_real_directory(&node_dir)?;
        let root = node_dir.join("root");
        create_real_directory(&root)?;
        create_real_directory(&root.join("zed_modules"))?;

        let mut entries = fs::read_dir(&node.source)
            .with_context(|| format!("failed to read immutable payload {}", node.source.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            ensure!(
                name != OsStr::new("zed_modules"),
                "package source `{}` contains reserved `zed_modules`",
                node.id
            );
            symlink(entry.path(), root.join(&name)).with_context(|| {
                format!(
                    "failed to link payload entry {} for `{}`",
                    entry.path().display(),
                    node.id
                )
            })?;
        }
        fs::write(node_dir.join("node.json"), node_record_bytes(node)?)?;
    }

    for edge in &plan.edges {
        let from = &plan.nodes[&edge.from];
        let to = &plan.nodes[&edge.to];
        let org_dir = staging
            .join("nodes")
            .join(&from.key)
            .join("root")
            .join("zed_modules")
            .join(&edge.to.org);
        if !org_dir.exists() {
            create_real_directory(&org_dir)?;
        }
        symlink(
            generation.join("nodes").join(&to.key).join("root"),
            org_dir.join(&edge.to.name),
        )
        .with_context(|| {
            format!(
                "failed to link exact dependency `{}` -> `{}`",
                edge.from, edge.to
            )
        })?;
    }

    fs::write(
        staging.join("graph.json"),
        plan.graph
            .canonical_document_bytes()
            .context("serializing canonical exact dependency graph")?,
    )?;
    fs::write(
        staging.join("cycles.json"),
        serde_json::to_vec_pretty(&plan.cycles)?,
    )?;
    fs::write(
        staging.join(".complete.json"),
        serde_json::to_vec_pretty(&GenerationMarker::for_plan(plan))?,
    )?;

    match fs::rename(staging, generation) {
        Ok(()) => Ok(()),
        Err(error) => {
            if path_is_real_directory(generation)? {
                remove_path(staging)?;
                verify_generation(generation, plan)
            } else {
                Err(error).with_context(|| {
                    format!(
                        "failed to publish graph generation {} -> {}",
                        staging.display(),
                        generation.display()
                    )
                })
            }
        }
    }
}

fn node_record_bytes(node: &ValidatedNode) -> Result<Vec<u8>> {
    let metadata = serde_json::json!({
        "id": &node.id,
        "artifact_digest": &node.artifact_digest,
        "source": &node.source,
        "node_key": &node.key,
    });
    serde_json::to_vec_pretty(&metadata).context("serializing exact node metadata")
}

#[cfg(unix)]
fn verify_generation(generation: &Path, plan: &ValidatedPlan) -> Result<()> {
    use std::ffi::OsStr;

    ensure!(
        path_is_real_directory(generation)?,
        "graph generation {} is not a real directory",
        generation.display()
    );
    let marker: GenerationMarker = serde_json::from_slice(
        &fs::read(generation.join(".complete.json")).with_context(|| {
            format!(
                "graph generation {} is incomplete; missing .complete.json marker",
                generation.display()
            )
        })?,
    )
    .with_context(|| format!("invalid generation marker in {}", generation.display()))?;
    ensure!(
        marker == GenerationMarker::for_plan(plan),
        "graph generation {} marker does not match the requested graph and source bindings",
        generation.display()
    );

    let expected_graph = plan
        .graph
        .canonical_document_bytes()
        .context("serializing canonical graph for generation verification")?;
    ensure!(
        fs::read(generation.join("graph.json"))? == expected_graph,
        "graph generation {} contains different graph bytes",
        generation.display()
    );
    let expected_cycles = serde_json::to_vec_pretty(&plan.cycles)?;
    ensure!(
        fs::read(generation.join("cycles.json"))? == expected_cycles,
        "graph generation {} contains different cycle diagnostics",
        generation.display()
    );

    for node in plan.nodes.values() {
        let node_dir = generation.join("nodes").join(&node.key);
        let root = node_dir.join("root");
        ensure!(
            path_is_real_directory(&root)?,
            "graph generation {} is missing exact node `{}`",
            generation.display(),
            node.id
        );
        ensure!(
            fs::read(node_dir.join("node.json"))? == node_record_bytes(node)?,
            "graph generation {} contains different metadata for exact node `{}`",
            generation.display(),
            node.id
        );
        let mut entries = fs::read_dir(&node.source)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            ensure!(
                name != OsStr::new("zed_modules"),
                "reserved payload entry appeared"
            );
            let link = root.join(&name);
            let metadata = fs::symlink_metadata(&link).with_context(|| {
                format!(
                    "graph generation is missing payload link {}",
                    link.display()
                )
            })?;
            ensure!(
                metadata.file_type().is_symlink(),
                "payload projection {} is not a symlink",
                link.display()
            );
            ensure!(
                fs::read_link(&link)? == entry.path(),
                "payload projection {} points at the wrong immutable source",
                link.display()
            );
        }
    }

    for edge in &plan.edges {
        let from = &plan.nodes[&edge.from];
        let to = &plan.nodes[&edge.to];
        let link = generation
            .join("nodes")
            .join(&from.key)
            .join("root")
            .join("zed_modules")
            .join(&edge.to.org)
            .join(&edge.to.name);
        let metadata = fs::symlink_metadata(&link)
            .with_context(|| format!("graph generation is missing edge link {}", link.display()))?;
        ensure!(
            metadata.file_type().is_symlink(),
            "exact dependency edge {} is not a symlink",
            link.display()
        );
        ensure!(
            fs::read_link(&link)? == generation.join("nodes").join(&to.key).join("root"),
            "exact dependency edge {} points at the wrong node",
            link.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
fn replace_directory_atomically(staging: &Path, destination: &Path, control: &Path) -> Result<()> {
    let backup = control.join(format!(".modules-backup-{}", Uuid::new_v4()));
    let had_destination = match fs::symlink_metadata(destination) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect existing modules {}",
                    destination.display()
                )
            });
        }
    };
    if had_destination {
        fs::rename(destination, &backup).with_context(|| {
            format!(
                "failed to move existing modules {} to transaction backup {}",
                destination.display(),
                backup.display()
            )
        })?;
    }
    if let Err(error) = fs::rename(staging, destination) {
        if had_destination {
            let _ = fs::rename(&backup, destination);
        }
        return Err(error).with_context(|| {
            format!(
                "failed to publish exact dependency modules {} -> {}",
                staging.display(),
                destination.display()
            )
        });
    }
    if had_destination {
        remove_path(&backup)?;
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect path {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path).with_context(|| format!("failed to remove path {}", path.display()))
    } else {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove directory {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use next_loggers::MemoryTransport;
    use tempfile::tempdir;
    use zed_interfaces::dependency_graph::{
        DependencyGraphCompleteness, DependencyGraphData, DependencyKind, RegistrySnapshot,
        ResolutionProvenance, ResolvedDependencyEdge, ResolvedDependencyNode,
    };

    use super::*;

    fn id(name: &str, version: &str) -> PackageVersionIdentity {
        PackageVersionIdentity {
            registry_id: "fixture-registry".into(),
            org: "fixture".into(),
            name: name.into(),
            version: version.into(),
        }
    }

    fn digest(seed: u8) -> String {
        format!("sha256:{}", format!("{seed:02x}").repeat(32))
    }

    fn node(id: PackageVersionIdentity, seed: u8) -> ResolvedDependencyNode {
        ResolvedDependencyNode {
            id,
            artifact_digest: Some(digest(seed)),
            features: Vec::new(),
        }
    }

    fn edge(from: PackageVersionIdentity, to: PackageVersionIdentity) -> ResolvedDependencyEdge {
        ResolvedDependencyEdge {
            from,
            to,
            kind: DependencyKind::Runtime,
            requirement: Some("exact fixture edge".into()),
            target: None,
            optional: false,
            features: Vec::new(),
        }
    }

    fn graph(
        roots: Vec<PackageVersionIdentity>,
        nodes: Vec<ResolvedDependencyNode>,
        edges: Vec<ResolvedDependencyEdge>,
    ) -> DependencyGraphDocument {
        DependencyGraphDocument {
            schema: zed_interfaces::dependency_graph::DEPENDENCY_GRAPH_SCHEMA_V1.into(),
            graph: DependencyGraphData::Resolved {
                completeness: DependencyGraphCompleteness::Complete,
                roots,
                nodes,
                edges,
                provenance: ResolutionProvenance {
                    resolver_version: "versioned-graph-test/v1".into(),
                    target: "test-target".into(),
                    enabled_features: Vec::new(),
                    registry_snapshots: vec![RegistrySnapshot {
                        registry_id: "fixture-registry".into(),
                        checkpoint_digest: digest(0xee),
                    }],
                    lock_digest: digest(0xdd),
                },
                parent_graph_digest: None,
                projection: None,
            },
            graph_digest: None,
        }
        .finalize()
        .unwrap()
    }

    fn canonical_cycle_graph() -> DependencyGraphDocument {
        let a1 = id("a", "1");
        let b1 = id("b", "1");
        let a2 = id("a", "2");
        let b0 = id("b", "0");
        graph(
            vec![a1.clone()],
            vec![
                node(a1.clone(), 1),
                node(b1.clone(), 2),
                node(a2.clone(), 3),
                node(b0.clone(), 4),
            ],
            vec![
                edge(a1, b1.clone()),
                edge(b1, a2.clone()),
                edge(a2.clone(), b0.clone()),
                edge(b0, a2),
            ],
        )
    }

    fn write_sources(root: &Path, graph: &DependencyGraphDocument) -> Vec<LocalPackageSource> {
        let DependencyGraphData::Resolved { nodes, .. } = &graph.graph else {
            unreachable!()
        };
        nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let source = root.join(format!("source-{index}"));
                fs::create_dir_all(&source).unwrap();
                fs::write(source.join("payload.txt"), format!("{}\n", node.id)).unwrap();
                LocalPackageSource {
                    id: node.id.clone(),
                    source,
                }
            })
            .collect()
    }

    #[test]
    fn exact_version_back_edge_terminates_without_conflating_a1_and_a2() {
        let graph = canonical_cycle_graph();
        let DependencyGraphData::Resolved {
            roots,
            nodes,
            edges,
            ..
        } = &graph.graph
        else {
            unreachable!()
        };
        let cycles = detect_cycle_paths(roots, nodes.iter().map(|node| &node.id), edges).unwrap();
        assert_eq!(nodes.len(), 4);
        assert_eq!(edges.len(), 4);
        assert_eq!(cycles.len(), 1);
        assert_eq!(
            cycles[0].path,
            vec![id("a", "2"), id("b", "0"), id("a", "2")]
        );
    }

    #[test]
    fn a_repeated_package_name_at_a_different_version_is_not_a_cycle() {
        let a1 = id("a", "1");
        let b1 = id("b", "1");
        let a2 = id("a", "2");
        let graph = graph(
            vec![a1.clone()],
            vec![
                node(a1.clone(), 1),
                node(b1.clone(), 2),
                node(a2.clone(), 3),
            ],
            vec![edge(a1, b1.clone()), edge(b1, a2)],
        );
        let DependencyGraphData::Resolved {
            roots,
            nodes,
            edges,
            ..
        } = &graph.graph
        else {
            unreachable!()
        };
        assert!(
            detect_cycle_paths(roots, nodes.iter().map(|node| &node.id), edges)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_shared_completed_node_is_not_reported_as_a_cycle() {
        let root = id("root", "1");
        let left = id("left", "1");
        let right = id("right", "1");
        let shared = id("shared", "1");
        let graph = graph(
            vec![root.clone()],
            vec![
                node(root.clone(), 1),
                node(left.clone(), 2),
                node(right.clone(), 3),
                node(shared.clone(), 4),
            ],
            vec![
                edge(root.clone(), left.clone()),
                edge(root, right.clone()),
                edge(left, shared.clone()),
                edge(right, shared),
            ],
        );
        let DependencyGraphData::Resolved {
            roots,
            nodes,
            edges,
            ..
        } = &graph.graph
        else {
            unreachable!()
        };
        assert!(
            detect_cycle_paths(roots, nodes.iter().map(|node| &node.id), edges)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn self_cycle_has_one_closed_exact_path() {
        let only = id("self", "1");
        let graph = graph(
            vec![only.clone()],
            vec![node(only.clone(), 1)],
            vec![edge(only.clone(), only.clone())],
        );
        let DependencyGraphData::Resolved {
            roots,
            nodes,
            edges,
            ..
        } = &graph.graph
        else {
            unreachable!()
        };
        let cycles = detect_cycle_paths(roots, nodes.iter().map(|node| &node.id), edges).unwrap();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].path, vec![only.clone(), only]);
    }

    #[test]
    fn two_node_same_version_cycle_is_reported_once() {
        let left = id("left", "1.0.0");
        let right = id("right", "1.0.0");
        let graph = graph(
            vec![left.clone()],
            vec![node(left.clone(), 1), node(right.clone(), 2)],
            vec![
                edge(left.clone(), right.clone()),
                edge(right.clone(), left.clone()),
            ],
        );
        let DependencyGraphData::Resolved {
            roots,
            nodes,
            edges,
            ..
        } = &graph.graph
        else {
            unreachable!()
        };
        let cycles = detect_cycle_paths(roots, nodes.iter().map(|node| &node.id), edges).unwrap();
        assert_eq!(cycles, vec![vec![left.clone(), right, left]]);
    }

    #[test]
    fn cycle_witnesses_are_deterministic_when_edges_arrive_in_another_order() {
        let graph = canonical_cycle_graph();
        let DependencyGraphData::Resolved {
            roots,
            nodes,
            edges,
            ..
        } = &graph.graph
        else {
            unreachable!()
        };
        let forward = detect_cycle_paths(roots, nodes.iter().map(|node| &node.id), edges).unwrap();
        let mut reversed = edges.clone();
        reversed.reverse();
        let backward =
            detect_cycle_paths(roots, nodes.iter().map(|node| &node.id), &reversed).unwrap();
        assert_eq!(forward, backward);
    }

    #[test]
    fn decorated_cycle_identity_includes_the_artifact_digest() {
        let temp = tempdir().unwrap();
        let graph = canonical_cycle_graph();
        let plan = LocalGraphMaterializationPlan {
            schema: LOCAL_MATERIALIZATION_SCHEMA_V1.into(),
            sources: write_sources(temp.path(), &graph),
            graph,
        };
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let validated =
            validate_plan(&plan, temp.path(), &project.canonicalize().unwrap()).unwrap();
        assert_eq!(validated.cycles.len(), 1);
        assert_eq!(validated.cycles[0].path[0].package, id("a", "2"));
        assert_eq!(validated.cycles[0].path[0].artifact_digest, digest(3));
        assert!(validated.cycles[0].cycle_id.starts_with("sha256:"));
    }

    #[cfg(unix)]
    #[test]
    fn materializes_one_finite_symlink_node_per_exact_version_and_logs_ore_event() {
        let temp = tempdir().unwrap();
        let graph = canonical_cycle_graph();
        let sources = write_sources(temp.path(), &graph);
        let plan = LocalGraphMaterializationPlan {
            schema: LOCAL_MATERIALIZATION_SCHEMA_V1.into(),
            graph,
            sources,
        };
        let transport = Arc::new(MemoryTransport::default());
        let logger = Logger::new(Options {
            app_name: "zed-cli-test".into(),
            console: false,
            ..Options::default().with_transport(transport.clone())
        });
        let project = temp.path().join("project");
        let first = materialize_plan(
            &plan,
            temp.path(),
            &project,
            VersionedMaterializationMode::Symlink,
            &logger,
        )
        .unwrap();
        assert_eq!(first.nodes, 4);
        assert_eq!(first.edges, 4);
        assert_eq!(first.cycles.len(), 1);
        assert!(!first.reused_generation);

        let records = transport.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].fields["event"], json!(CYCLE_EVENT));
        assert_eq!(records[0].fields["resolution"], json!(RESOLUTION_MODE));
        assert_eq!(
            records[0].fields["materialization_digest"],
            json!(&first.materialization_digest)
        );
        assert!(records[0].message.contains("fixture-registry::fixture/a@2"));
        assert!(records[0].message.contains("fixture-registry::fixture/b@0"));

        let node_dirs = fs::read_dir(first.generation_dir.join("nodes"))
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(node_dirs.len(), 4);
        for entry in node_dirs {
            let payload = entry.path().join("root/payload.txt");
            assert!(
                fs::symlink_metadata(payload)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }

        let root = project.join("zed_modules/fixture/a");
        let b1 = root.join("zed_modules/fixture/b");
        let a2 = b1.join("zed_modules/fixture/a");
        let b0 = a2.join("zed_modules/fixture/b");
        let closing_a2 = b0.join("zed_modules/fixture/a");
        assert_eq!(
            fs::read_link(&closing_a2).unwrap(),
            a2.canonicalize().unwrap()
        );

        let second = materialize_plan(
            &plan,
            temp.path(),
            &project,
            VersionedMaterializationMode::Symlink,
            &logger,
        )
        .unwrap();
        assert!(second.reused_generation);
        assert_eq!(first.generation_dir, second.generation_dir);
        assert_eq!(
            fs::read_dir(first.generation_dir.parent().unwrap())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
                })
                .count(),
            1
        );
    }

    #[test]
    fn store_binding_uses_each_nodes_content_addressed_package_directory() {
        let temp = tempdir().unwrap();
        let graph = canonical_cycle_graph();
        let store = Store::new(&temp.path().join("home"));
        let DependencyGraphData::Resolved { nodes, .. } = &graph.graph else {
            unreachable!()
        };
        for node in nodes {
            let digest = node.artifact_digest.as_deref().unwrap();
            let sha256 = digest.strip_prefix("sha256:").unwrap();
            fs::create_dir_all(store.pkg_dir(sha256)).unwrap();
        }
        let plan = plan_from_store(graph, &store).unwrap();
        assert_eq!(plan.sources.len(), 4);
        for source in plan.sources {
            assert!(source.source.starts_with(store.root()));
            assert!(source.source.ends_with("pkg"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn changing_source_bindings_creates_another_generation_without_changing_graph_identity() {
        let temp = tempdir().unwrap();
        let graph = canonical_cycle_graph();
        let first_source_root = temp.path().join("first-sources");
        let second_source_root = temp.path().join("second-sources");
        fs::create_dir_all(&first_source_root).unwrap();
        fs::create_dir_all(&second_source_root).unwrap();
        let first_plan = LocalGraphMaterializationPlan {
            schema: LOCAL_MATERIALIZATION_SCHEMA_V1.into(),
            graph: graph.clone(),
            sources: write_sources(&first_source_root, &graph),
        };
        let second_plan = LocalGraphMaterializationPlan {
            schema: LOCAL_MATERIALIZATION_SCHEMA_V1.into(),
            graph,
            sources: write_sources(&second_source_root, &first_plan.graph),
        };
        let logger = Logger::new(Options {
            console: false,
            ..Options::default()
        });
        let project = temp.path().join("project");
        let first = materialize_plan(
            &first_plan,
            temp.path(),
            &project,
            VersionedMaterializationMode::Symlink,
            &logger,
        )
        .unwrap();
        let second = materialize_plan(
            &second_plan,
            temp.path(),
            &project,
            VersionedMaterializationMode::Symlink,
            &logger,
        )
        .unwrap();
        assert_eq!(first.graph_digest, second.graph_digest);
        assert_ne!(first.materialization_digest, second.materialization_digest);
        assert_ne!(first.generation_dir, second.generation_dir);
        assert!(first.generation_dir.is_dir());
        assert!(second.generation_dir.is_dir());
    }

    #[cfg(not(unix))]
    #[test]
    fn symlink_mode_fails_explicitly_when_directory_symlinks_are_unavailable() {
        let temp = tempdir().unwrap();
        let graph = canonical_cycle_graph();
        let plan = LocalGraphMaterializationPlan {
            schema: LOCAL_MATERIALIZATION_SCHEMA_V1.into(),
            sources: write_sources(temp.path(), &graph),
            graph,
        };
        let logger = Logger::new(Options {
            console: false,
            ..Options::default()
        });
        let project = temp.path().join("project");
        let error = materialize_plan(
            &plan,
            temp.path(),
            &project,
            VersionedMaterializationMode::Symlink,
            &logger,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("requires directory symlinks"));
        assert!(!project.join("zed_modules").exists());
    }

    #[test]
    fn copy_mode_fails_closed_before_recursive_mirroring() {
        let temp = tempdir().unwrap();
        let graph = canonical_cycle_graph();
        let sources = write_sources(temp.path(), &graph);
        let plan = LocalGraphMaterializationPlan {
            schema: LOCAL_MATERIALIZATION_SCHEMA_V1.into(),
            graph,
            sources,
        };
        let logger = Logger::new(Options {
            console: false,
            ..Options::default()
        });
        let error = materialize_plan(
            &plan,
            temp.path(),
            &temp.path().join("project"),
            VersionedMaterializationMode::Copy,
            &logger,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot represent a circular exact-version dependency graph"));
    }
}
