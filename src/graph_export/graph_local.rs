//! Prospective dependency-graph resolution for a local `.zpkg.toml`.
//!
//! This path is read-only with respect to the project. It prefers immutable
//! declared-graph metadata from the registry so AI and other tooling can
//! calculate a complete prospective graph without downloading every package
//! artifact just to discover its manifest. Older/file registries can use an
//! explicit verified artifact-manifest fallback when requested.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use globset::Glob;
use serde::Serialize;
use sha2::{Digest, Sha256};
use zed_interfaces::dependency_graph::{
    DEPENDENCY_GRAPH_DEFAULT_MAX_EDGES, DEPENDENCY_GRAPH_DEFAULT_MAX_ENCODED_BYTES,
    DEPENDENCY_GRAPH_DEFAULT_MAX_NODES, DEPENDENCY_GRAPH_DIGEST_HEADER, DependencyGraphData,
    DependencyGraphDocument, DependencyKind,
};
use zed_interfaces::manifest::{Manifest, is_slug};
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::registry::{PackageMetadata, VersionMetadata};
use zed_interfaces::version::{self, VersionScheme};
use zed_lib::requirement_matches;

use crate::config::{Config, read_manifest};
use crate::install_graph::ensure_artifact;
use crate::registry::{Registry, registry_for};
use crate::store::Store;

const ANALYSIS_SCHEMA: &str = "zpkg/local-dependency-graph/v1";
const MAX_DEPENDENCY_DEPTH: usize = 256;
const MAX_GRAPH_COORDINATES: usize = 10_000;
const MAX_ERROR_PATHS: usize = 32;
const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub(super) struct LocalGraphOptions {
    pub(super) manifest: PathBuf,
    pub(super) output: Option<PathBuf>,
    pub(super) pretty: bool,
    pub(super) runtime_only: bool,
    pub(super) allow_artifact_fallback: bool,
    pub(super) max_metadata_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct LocalDependencyGraph {
    schema: String,
    complete: bool,
    root: GraphNode,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    stats: GraphStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    analysis_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct GraphNode {
    package: String,
    version: String,
    source: NodeSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum NodeSource {
    Root,
    Registry,
    Workspace,
    PathOverride,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct GraphEdge {
    from: String,
    to: String,
    requirement: String,
    kind: DependencyKind,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
struct GraphStats {
    registry_package_reads: usize,
    registry_version_reads: usize,
    declared_graph_reads: usize,
    declared_graph_cache_hits: usize,
    artifact_manifest_fallbacks: usize,
    artifact_downloads: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DependencySpec {
    key: String,
    requirement: String,
    kind: DependencyKind,
}

#[derive(Debug, Clone)]
struct Candidate {
    key: String,
    version: String,
    scheme: VersionScheme,
    dependencies: Vec<DependencySpec>,
    source: NodeSource,
    artifact_sha256: Option<String>,
    yanked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Constraint {
    requirement: String,
    path: Vec<String>,
    propagate: bool,
}

#[derive(Debug, Clone, Default)]
struct SolveState {
    constraints: BTreeMap<String, Vec<Constraint>>,
    selected: BTreeMap<String, Candidate>,
}

impl SolveState {
    fn add_constraint(&mut self, key: String, constraint: Constraint) -> Result<bool> {
        split_key(&key)?;
        let depth = constraint.path.len().saturating_sub(1);
        if depth > MAX_DEPENDENCY_DEPTH {
            bail!(
                "dependency graph exceeds the maximum depth of {MAX_DEPENDENCY_DEPTH} while resolving `{key}` via {}",
                render_path(&constraint.path)
            );
        }
        if !self.constraints.contains_key(&key) && self.constraints.len() >= MAX_GRAPH_COORDINATES {
            bail!(
                "dependency graph exceeds the {MAX_GRAPH_COORDINATES}-coordinate limit while adding `{key}`; refusing"
            );
        }
        let constraints = self.constraints.entry(key).or_default();
        if constraints.contains(&constraint) {
            return Ok(false);
        }
        constraints.push(constraint);
        constraints.sort();
        Ok(true)
    }

    fn unresolved_key(&self) -> Option<String> {
        self.constraints
            .iter()
            .filter(|(key, _)| !self.selected.contains_key(*key))
            .min_by(|(left_key, left), (right_key, right)| {
                constraint_depth(left)
                    .cmp(&constraint_depth(right))
                    .then_with(|| left_key.cmp(right_key))
            })
            .map(|(key, _)| key.clone())
    }
}

fn constraint_depth(constraints: &[Constraint]) -> usize {
    constraints
        .iter()
        .map(|constraint| constraint.path.len())
        .min()
        .unwrap_or(usize::MAX)
}

fn render_path(path: &[String]) -> String {
    const HEAD: usize = 4;
    const TAIL: usize = 8;
    if path.len() <= HEAD + TAIL {
        return path.join(" -> ");
    }
    let omitted = path.len() - HEAD - TAIL;
    format!(
        "{} -> ... {omitted} segment(s) omitted ... -> {}",
        path[..HEAD].join(" -> "),
        path[path.len() - TAIL..].join(" -> ")
    )
}

trait SolveSource {
    fn package(&mut self, org: &str, name: &str) -> Result<PackageMetadata>;
    fn candidate(
        &mut self,
        key: &str,
        org: &str,
        name: &str,
        version: &str,
        scheme: VersionScheme,
    ) -> Result<Candidate>;
}

struct AnalyzerSource<'a> {
    cfg: &'a Config,
    registry: Box<dyn Registry>,
    store: Store,
    token: Option<String>,
    http: reqwest::blocking::Client,
    runtime_only: bool,
    allow_artifact_fallback: bool,
    max_metadata_bytes: u64,
    packages: BTreeMap<String, PackageMetadata>,
    candidates: BTreeMap<(String, String), Candidate>,
    declared_graphs: BTreeMap<(String, String), Vec<DependencySpec>>,
    stats: GraphStats,
}

impl<'a> AnalyzerSource<'a> {
    fn new(
        cfg: &'a Config,
        runtime_only: bool,
        allow_artifact_fallback: bool,
        max_metadata_bytes: u64,
    ) -> Result<Self> {
        ensure!(
            max_metadata_bytes > 0
                && max_metadata_bytes <= DEPENDENCY_GRAPH_DEFAULT_MAX_ENCODED_BYTES,
            "--max-metadata-bytes must be between 1 and {}",
            DEPENDENCY_GRAPH_DEFAULT_MAX_ENCODED_BYTES
        );
        Ok(Self {
            cfg,
            registry: registry_for(&cfg.registry)?,
            store: Store::new(&cfg.home),
            token: cfg.resolve_token()?,
            http: reqwest::blocking::Client::builder()
                .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
                .redirect(reqwest::redirect::Policy::none())
                .timeout(METADATA_REQUEST_TIMEOUT)
                .build()?,
            runtime_only,
            allow_artifact_fallback,
            max_metadata_bytes,
            packages: BTreeMap::new(),
            candidates: BTreeMap::new(),
            declared_graphs: BTreeMap::new(),
            stats: GraphStats::default(),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn declared_dependencies(
        &mut self,
        org: &str,
        name: &str,
        version: &str,
    ) -> Result<Option<Vec<DependencySpec>>> {
        let cache_key = (format!("{org}/{name}"), version.to_string());
        if let Some(dependencies) = self.declared_graphs.get(&cache_key) {
            self.stats.declared_graph_cache_hits += 1;
            return Ok(Some(dependencies.clone()));
        }

        let mut url = match reqwest::Url::parse(&self.cfg.registry) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => url,
            _ => return Ok(None),
        };
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("registry URL must not contain credentials, a query, or a fragment");
        }
        ensure!(
            url.scheme() == "https" || url_is_loopback(&url),
            "fast dependency-graph metadata requires HTTPS outside loopback registries"
        );
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow::anyhow!("registry URL cannot be a base URL"))?;
            segments.pop_if_empty();
            for segment in [
                "v1",
                "packages",
                org,
                name,
                "versions",
                version,
                "dependency-graph",
            ] {
                segments.push(segment);
            }
        }
        url.query_pairs_mut()
            .append_pair("view", "declared")
            .append_pair("format", "json");

        let mut request = self.http.get(url.clone()).header(
            "Accept",
            zed_interfaces::dependency_graph::DEPENDENCY_GRAPH_JSON_MEDIA_TYPE,
        );
        if let Some(token) = self.token.as_deref() {
            request = request.bearer_auth(token);
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(_error) if self.allow_artifact_fallback => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("requesting declared dependency graph for {org}/{name}@{version}")
                });
            }
        };
        if matches!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::METHOD_NOT_ALLOWED
                | reqwest::StatusCode::NOT_IMPLEMENTED
        ) {
            return Ok(None);
        }
        if !response.status().is_success() {
            if self.allow_artifact_fallback {
                return Ok(None);
            }
            bail!(
                "declared dependency graph request for {org}/{name}@{version} failed with HTTP {}",
                response.status()
            );
        }
        if let Some(length) = response.content_length() {
            ensure!(
                length <= self.max_metadata_bytes,
                "declared dependency graph for {org}/{name}@{version} declares {length} bytes, above the {} byte analysis limit",
                self.max_metadata_bytes
            );
        }
        let header_digest = response
            .headers()
            .get(DEPENDENCY_GRAPH_DIGEST_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        let mut body = Vec::new();
        response
            .take(self.max_metadata_bytes.saturating_add(1))
            .read_to_end(&mut body)
            .with_context(|| {
                format!("reading declared dependency graph for {org}/{name}@{version}")
            })?;
        ensure!(
            body.len() as u64 <= self.max_metadata_bytes,
            "declared dependency graph for {org}/{name}@{version} exceeds the {} byte analysis limit",
            self.max_metadata_bytes
        );
        let document = DependencyGraphDocument::parse_verified_canonical(&body).with_context(|| {
            format!("verifying declared dependency graph for {org}/{name}@{version}")
        })?;
        if let Some(header_digest) = header_digest {
            ensure!(
                document.graph_digest.as_deref() == Some(header_digest.as_str()),
                "declared dependency graph header digest disagrees with the verified document for {org}/{name}@{version}"
            );
        }
        let (package, dependencies) = match document.graph {
            DependencyGraphData::Declared {
                package,
                dependencies,
            } => (package, dependencies),
            DependencyGraphData::Resolved { .. } => {
                bail!(
                    "registry returned a resolved graph for declared package route {org}/{name}@{version}"
                )
            }
        };
        ensure!(
            package.org == org && package.name == name && package.version == version,
            "declared dependency graph identity mismatch: requested {org}/{name}@{version}, received {package}"
        );

        let mut specs = Vec::new();
        for dependency in dependencies {
            if self.runtime_only && dependency.kind != DependencyKind::Runtime {
                continue;
            }
            if dependency.optional {
                continue;
            }
            ensure!(
                dependency.registry_id == package.registry_id,
                "cross-registry dependency `{}/{}` from {org}/{name}@{version} requires an explicit registry-aware resolver",
                dependency.org,
                dependency.name
            );
            specs.push(DependencySpec {
                key: format!("{}/{}", dependency.org, dependency.name),
                requirement: dependency.requirement,
                kind: dependency.kind,
            });
        }
        specs.sort();
        specs.dedup();
        self.stats.declared_graph_reads += 1;
        self.declared_graphs.insert(cache_key, specs.clone());
        Ok(Some(specs))
    }

    fn artifact_dependencies(
        &mut self,
        key: &str,
        version: &VersionMetadata,
    ) -> Result<Vec<DependencySpec>> {
        let (package_dir, downloaded) =
            ensure_artifact(self.registry.as_ref(), &self.store, version).with_context(|| {
                format!(
                    "loading artifact manifest fallback for {key}@{}",
                    version.version
                )
            })?;
        self.stats.artifact_downloads += usize::from(downloaded);
        let manifest_path = package_dir.join(MANIFEST_FILE);
        if !manifest_path.is_file() {
            self.stats.artifact_manifest_fallbacks += 1;
            return Ok(Vec::new());
        }
        let manifest = read_manifest(&package_dir).with_context(|| {
            format!(
                "reading artifact dependency manifest for {key}@{} from {}",
                version.version,
                manifest_path.display()
            )
        })?;
        ensure!(
            manifest.full_name() == key && manifest.package.version == version.version,
            "artifact manifest declares {}@{} while registry metadata selected {key}@{}",
            manifest.full_name(),
            manifest.package.version,
            version.version
        );
        self.stats.artifact_manifest_fallbacks += 1;
        Ok(manifest_dependencies(&manifest, self.runtime_only))
    }
}

impl SolveSource for AnalyzerSource<'_> {
    fn package(&mut self, org: &str, name: &str) -> Result<PackageMetadata> {
        let key = format!("{org}/{name}");
        if let Some(package) = self.packages.get(&key) {
            return Ok(package.clone());
        }
        let package = self.registry.get_package(org, name)?;
        ensure!(
            package.org == org && package.name == name,
            "registry returned package `{}/{}` while resolving `{key}`",
            package.org,
            package.name
        );
        self.stats.registry_package_reads += 1;
        self.packages.insert(key, package.clone());
        Ok(package)
    }

    fn candidate(
        &mut self,
        key: &str,
        org: &str,
        name: &str,
        version: &str,
        scheme: VersionScheme,
    ) -> Result<Candidate> {
        let cache_key = (key.to_string(), version.to_string());
        if let Some(candidate) = self.candidates.get(&cache_key) {
            return Ok(candidate.clone());
        }
        let metadata = self.registry.get_version(org, name, version)?;
        ensure!(
            metadata.org == org && metadata.name == name && metadata.version == version,
            "registry returned `{}/{}@{}` while resolving `{key}@{version}`",
            metadata.org,
            metadata.name,
            metadata.version
        );
        self.stats.registry_version_reads += 1;
        let dependencies = if metadata.yanked {
            Vec::new()
        } else {
            match self.declared_dependencies(org, name, version)? {
                Some(dependencies) => dependencies,
                None if self.allow_artifact_fallback => {
                    self.artifact_dependencies(key, &metadata)?
                }
                None => bail!(
                    "declared dependency graph metadata is unavailable for {key}@{version}; refusing artifact download in fast analysis mode (pass --allow-artifact-fallback to permit verified artifact-manifest fallback)"
                ),
            }
        };
        let candidate = Candidate {
            key: key.to_string(),
            version: version.to_string(),
            scheme,
            dependencies,
            source: NodeSource::Registry,
            artifact_sha256: Some(metadata.sha256),
            yanked: metadata.yanked,
        };
        self.candidates.insert(cache_key, candidate.clone());
        Ok(candidate)
    }
}

fn url_is_loopback(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some(host) if host.eq_ignore_ascii_case("localhost") => true,
        Some(host) => host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

fn manifest_dependencies(manifest: &Manifest, runtime_only: bool) -> Vec<DependencySpec> {
    let mut dependencies = manifest
        .dependencies
        .iter()
        .map(|(key, requirement)| DependencySpec {
            key: key.clone(),
            requirement: requirement.clone(),
            kind: DependencyKind::Runtime,
        })
        .collect::<Vec<_>>();
    if !runtime_only {
        dependencies.extend(
            manifest
                .build_dependencies
                .iter()
                .map(|(key, requirement)| DependencySpec {
                    key: key.clone(),
                    requirement: requirement.clone(),
                    kind: DependencyKind::Build,
                }),
        );
    }
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

fn local_candidate(manifest: Manifest, source: NodeSource, runtime_only: bool) -> Candidate {
    Candidate {
        key: manifest.full_name(),
        version: manifest.package.version.clone(),
        scheme: manifest.package.version_scheme,
        dependencies: manifest_dependencies(&manifest, runtime_only),
        source,
        artifact_sha256: None,
        yanked: false,
    }
}

fn discover_local_candidates(
    project: &Path,
    root_manifest: &Manifest,
    runtime_only: bool,
) -> Result<BTreeMap<String, Candidate>> {
    let mut candidates = BTreeMap::new();
    let mut current = Some(project);
    while let Some(directory) = current {
        if directory.join(MANIFEST_FILE).is_file() {
            let manifest = read_manifest(directory)?;
            if let Some(workspace) = manifest.workspace.as_ref() {
                for pattern in &workspace.members {
                    for member_dir in expand_workspace_pattern(directory, pattern)? {
                        let member = read_manifest(&member_dir).with_context(|| {
                            format!("reading workspace member {}", member_dir.display())
                        })?;
                        let candidate =
                            local_candidate(member, NodeSource::Workspace, runtime_only);
                        candidates.insert(candidate.key.clone(), candidate);
                    }
                }
                break;
            }
        }
        current = directory.parent();
    }

    let raw_overrides = crate::local_overrides::read(project)?;
    if !raw_overrides.is_empty() {
        let resolved = crate::local_overrides::resolve(
            project,
            root_manifest.modules_dir(),
            &raw_overrides,
        )?;
        for (key, directory) in resolved {
            let manifest = read_manifest(&directory).with_context(|| {
                format!(
                    "reading local path override `{key}` from {}",
                    directory.display()
                )
            })?;
            ensure!(
                manifest.full_name() == key,
                "local path override `{key}` points to package `{}` at {}",
                manifest.full_name(),
                directory.display()
            );
            candidates.insert(
                key,
                local_candidate(manifest, NodeSource::PathOverride, runtime_only),
            );
        }
    }

    let root_key = root_manifest.full_name();
    if root_manifest.dependencies.contains_key(&root_key)
        || (!runtime_only && root_manifest.build_dependencies.contains_key(&root_key))
    {
        candidates.remove(&root_key);
    }
    Ok(candidates)
}

fn expand_workspace_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>> {
    let mut candidates = vec![root.to_path_buf()];
    for segment in pattern.split('/') {
        let mut next = Vec::new();
        for base in &candidates {
            if segment.contains('*') {
                let glob = Glob::new(segment)
                    .with_context(|| format!("invalid workspace glob segment `{segment}`"))?;
                let matcher = glob.compile_matcher();
                let entries = match fs::read_dir(base) {
                    Ok(entries) => entries,
                    Err(_) => continue,
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if entry.path().is_dir()
                        && matcher.is_match(Path::new(&name))
                        && !name.to_string_lossy().starts_with('.')
                    {
                        next.push(entry.path());
                    }
                }
            } else {
                let candidate = base.join(segment);
                if candidate.is_dir() {
                    next.push(candidate);
                }
            }
        }
        candidates = next;
    }
    candidates.sort();
    candidates.dedup();
    Ok(candidates)
}

enum SearchOutcome {
    Solved(SolveState),
    Unsatisfiable(String),
}

struct Solver<'a, S> {
    source: &'a mut S,
    locals: &'a BTreeMap<String, Candidate>,
}

impl<S: SolveSource> Solver<'_, S> {
    fn solve(&mut self, mut state: SolveState) -> Result<SearchOutcome> {
        if let Some(conflict) = self.propagate(&mut state)? {
            return Ok(SearchOutcome::Unsatisfiable(conflict));
        }
        let Some(key) = state.unresolved_key() else {
            return Ok(SearchOutcome::Solved(state));
        };
        let constraints = state.constraints.get(&key).cloned().unwrap_or_default();

        if let Some(local) = self.locals.get(&key) {
            if constraints.iter().any(|constraint| {
                !requirement_matches(local.scheme, &constraint.requirement, &local.version)
            }) {
                return Ok(SearchOutcome::Unsatisfiable(render_conflict(
                    &key,
                    &constraints,
                    Some(&local.version),
                    &[],
                )));
            }
            state.selected.insert(key, local.clone());
            return self.solve(state);
        }

        let (org, name) = split_key(&key)?;
        let package = self.source.package(org, name)?;
        let mut versions = package.versions.clone();
        version::sort_desc(&mut versions);
        let mut failures = Vec::new();
        let mut matching = false;
        let mut saw_non_yanked = false;

        for published in &versions {
            if constraints.iter().any(|constraint| {
                !requirement_matches(package.version_scheme, &constraint.requirement, published)
            }) {
                continue;
            }
            matching = true;
            // Registry/auth/metadata/integrity failures are operational errors,
            // not evidence that this version is semantically unsatisfiable.
            // Bubble them out rather than silently trying an older package.
            let candidate =
                self.source
                    .candidate(&key, org, name, published, package.version_scheme)?;
            if candidate.yanked {
                continue;
            }
            saw_non_yanked = true;
            let mut branch = state.clone();
            branch.selected.insert(key.clone(), candidate);
            match self.solve(branch)? {
                SearchOutcome::Solved(solved) => return Ok(SearchOutcome::Solved(solved)),
                SearchOutcome::Unsatisfiable(failure) => {
                    failures.push((published.clone(), failure));
                }
            }
        }

        if matching && !saw_non_yanked {
            return Ok(SearchOutcome::Unsatisfiable(format!(
                "version conflict for {key}: all matching versions are yanked; use an existing lock with `zed install --frozen` to replay a previously selected version"
            )));
        }
        Ok(SearchOutcome::Unsatisfiable(render_conflict(
            &key,
            &constraints,
            None,
            &failures,
        )))
    }

    fn propagate(&self, state: &mut SolveState) -> Result<Option<String>> {
        loop {
            for (key, selected) in &state.selected {
                let constraints = state.constraints.get(key).cloned().unwrap_or_default();
                if constraints.iter().any(|constraint| {
                    !requirement_matches(selected.scheme, &constraint.requirement, &selected.version)
                }) {
                    return Ok(Some(render_conflict(
                        key,
                        &constraints,
                        Some(&selected.version),
                        &[],
                    )));
                }
            }

            let mut additions = Vec::new();
            for (key, selected) in &state.selected {
                let parents = state.constraints.get(key).cloned().unwrap_or_default();
                for parent in parents {
                    if !parent.propagate {
                        continue;
                    }
                    for dependency in &selected.dependencies {
                        additions.push((
                            dependency.key.clone(),
                            child_constraint(
                                &parent,
                                key,
                                &selected.version,
                                &dependency.key,
                                &dependency.requirement,
                            ),
                        ));
                    }
                }
            }

            let mut changed = false;
            for (key, constraint) in additions {
                changed |= state.add_constraint(key, constraint)?;
            }
            if !changed {
                return Ok(None);
            }
        }
    }
}

fn child_constraint(
    parent: &Constraint,
    parent_key: &str,
    parent_version: &str,
    dependency: &str,
    requirement: &str,
) -> Constraint {
    let cycle_back_edge = parent.path.iter().any(|segment| {
        segment
            .split_once('@')
            .map_or(segment.as_str(), |(coordinate, _)| coordinate)
            == dependency
    });
    let mut path = parent.path.clone();
    if let Some(last) = path.last_mut() {
        *last = format!("{parent_key}@{parent_version}");
    }
    path.push(dependency.to_string());
    Constraint {
        requirement: requirement.to_string(),
        path,
        propagate: !cycle_back_edge,
    }
}

fn render_conflict(
    key: &str,
    constraints: &[Constraint],
    selected: Option<&str>,
    failures: &[(String, String)],
) -> String {
    let mut lines = vec![match selected {
        Some(version) => format!(
            "version conflict for {key}: selected {version}, but it does not satisfy every active requirement"
        ),
        None => format!(
            "version conflict for {key}: no version satisfies every active requirement"
        ),
    }];
    for constraint in constraints.iter().take(MAX_ERROR_PATHS) {
        lines.push(format!(
            "  - `{}` via {}",
            constraint.requirement,
            render_path(&constraint.path)
        ));
    }
    if constraints.len() > MAX_ERROR_PATHS {
        lines.push(format!(
            "  - ... {} additional requirement paths omitted",
            constraints.len() - MAX_ERROR_PATHS
        ));
    }
    for (version, error) in failures.iter().take(8) {
        lines.push(format!(
            "  candidate {version} led to: {}",
            error.lines().next().unwrap_or("unknown conflict")
        ));
    }
    lines.join("\n")
}

fn split_key(key: &str) -> Result<(&str, &str)> {
    let Some((org, name)) = key.split_once('/') else {
        bail!("invalid package spec `{key}` (expected org/name)");
    };
    ensure!(
        is_slug(org) && is_slug(name) && !name.contains('/'),
        "invalid package spec `{key}` (expected slug/slug without path traversal or extra segments)"
    );
    Ok((org, name))
}

fn build_graph(
    manifest: &Manifest,
    state: SolveState,
    stats: GraphStats,
    runtime_only: bool,
) -> Result<LocalDependencyGraph> {
    let root_key = manifest.full_name();
    let root = GraphNode {
        package: root_key.clone(),
        version: manifest.package.version.clone(),
        source: NodeSource::Root,
        artifact_sha256: None,
    };
    let mut nodes = vec![root.clone()];
    nodes.extend(
        state
            .selected
            .values()
            .filter(|candidate| {
                candidate.key != root_key || candidate.version != manifest.package.version
            })
            .map(|candidate| GraphNode {
                package: candidate.key.clone(),
                version: candidate.version.clone(),
                source: candidate.source,
                artifact_sha256: candidate.artifact_sha256.clone(),
            })
            .collect::<Vec<_>>(),
    );
    nodes.sort();
    nodes.dedup();

    let mut edges = Vec::new();
    let root_dependencies = manifest_dependencies(manifest, runtime_only);
    for dependency in root_dependencies {
        if let Some(selected) = state.selected.get(&dependency.key) {
            edges.push(GraphEdge {
                from: format!("{}@{}", root_key, manifest.package.version),
                to: format!("{}@{}", dependency.key, selected.version),
                requirement: dependency.requirement,
                kind: dependency.kind,
            });
        }
    }
    for candidate in state.selected.values() {
        for dependency in &candidate.dependencies {
            if let Some(selected) = state.selected.get(&dependency.key) {
                edges.push(GraphEdge {
                    from: format!("{}@{}", candidate.key, candidate.version),
                    to: format!("{}@{}", dependency.key, selected.version),
                    requirement: dependency.requirement.clone(),
                    kind: dependency.kind,
                });
            } else if dependency.key == root_key {
                edges.push(GraphEdge {
                    from: format!("{}@{}", candidate.key, candidate.version),
                    to: format!("{}@{}", root_key, manifest.package.version),
                    requirement: dependency.requirement.clone(),
                    kind: dependency.kind,
                });
            }
        }
    }
    edges.sort();
    edges.dedup();
    ensure!(
        nodes.len() <= DEPENDENCY_GRAPH_DEFAULT_MAX_NODES as usize,
        "resolved graph exceeds the {} node limit",
        DEPENDENCY_GRAPH_DEFAULT_MAX_NODES
    );
    ensure!(
        edges.len() <= DEPENDENCY_GRAPH_DEFAULT_MAX_EDGES as usize,
        "resolved graph exceeds the {} edge limit",
        DEPENDENCY_GRAPH_DEFAULT_MAX_EDGES
    );

    let mut graph = LocalDependencyGraph {
        schema: ANALYSIS_SCHEMA.to_string(),
        complete: true,
        root,
        nodes,
        edges,
        stats,
        analysis_digest: None,
    };
    #[derive(Serialize)]
    struct SemanticGraph<'a> {
        schema: &'a str,
        complete: bool,
        root: &'a GraphNode,
        nodes: &'a [GraphNode],
        edges: &'a [GraphEdge],
    }
    let payload = serde_json::to_vec(&SemanticGraph {
        schema: &graph.schema,
        complete: graph.complete,
        root: &graph.root,
        nodes: &graph.nodes,
        edges: &graph.edges,
    })
    .context("serializing local graph semantic digest payload")?;
    graph.analysis_digest = Some(format!(
        "sha256:{}",
        hex::encode(Sha256::digest(payload))
    ));
    Ok(graph)
}

fn manifest_project(manifest_path: &Path) -> Result<(PathBuf, Manifest)> {
    let path = if manifest_path.is_dir() {
        manifest_path.join(MANIFEST_FILE)
    } else {
        manifest_path.to_path_buf()
    };
    ensure!(
        path.file_name().is_some_and(|name| name == MANIFEST_FILE),
        "--manifest must name `{MANIFEST_FILE}` or a directory containing it"
    );
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let project = fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing manifest directory {}", parent.display()))?;
    let manifest = read_manifest(&project)?;
    Ok((project, manifest))
}

fn resolve(config: &Config, options: &LocalGraphOptions) -> Result<LocalDependencyGraph> {
    let (project, manifest) = manifest_project(&options.manifest)?;
    let locals = discover_local_candidates(&project, &manifest, options.runtime_only)?;
    let mut state = SolveState::default();
    let root = format!("{}@{}", manifest.full_name(), manifest.package.version);
    for dependency in manifest_dependencies(&manifest, options.runtime_only) {
        state.add_constraint(
            dependency.key.clone(),
            Constraint {
                requirement: dependency.requirement,
                path: vec![root.clone(), dependency.key],
                propagate: true,
            },
        )?;
    }

    let mut source = AnalyzerSource::new(
        config,
        options.runtime_only,
        options.allow_artifact_fallback,
        options.max_metadata_bytes,
    )?;
    let solved = match (Solver {
        source: &mut source,
        locals: &locals,
    })
    .solve(state)?
    {
        SearchOutcome::Solved(solved) => solved,
        SearchOutcome::Unsatisfiable(failure) => bail!(failure),
    };
    build_graph(&manifest, solved, source.stats, options.runtime_only)
}

pub(super) fn run(config: &Config, options: LocalGraphOptions) -> Result<i32> {
    let graph = resolve(config, &options)?;
    let bytes = if options.pretty {
        let mut bytes = serde_json::to_vec_pretty(&graph)?;
        bytes.push(b'\n');
        bytes
    } else {
        let mut bytes = serde_json::to_vec(&graph)?;
        bytes.push(b'\n');
        bytes
    };
    write_output(options.output.as_deref(), &bytes)?;
    Ok(0)
}

fn write_output(path: Option<&Path>, bytes: &[u8]) -> Result<()> {
    match path {
        None => write_stdout(bytes),
        Some(path) if path == Path::new("-") => write_stdout(bytes),
        Some(path) => write_atomic_file(path, bytes),
    }
}

fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(bytes)
        .context("writing local dependency graph to stdout")?;
    stdout
        .flush()
        .context("flushing local dependency graph stdout")?;
    Ok(())
}

fn write_atomic_file(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty(),
        "local dependency graph output path may not be empty"
    );
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "local dependency graph output already exists: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "checking local dependency graph output {}",
                    path.display()
                )
            });
        }
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata = fs::metadata(parent)
        .with_context(|| format!("reading output directory {}", parent.display()))?;
    ensure!(
        metadata.is_dir(),
        "local dependency graph output parent is not a directory: {}",
        parent.display()
    );
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating atomic output beside {}", path.display()))?;
    temporary
        .write_all(bytes)
        .with_context(|| format!("writing temporary graph output for {}", path.display()))?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("syncing temporary graph output for {}", path.display()))?;
    temporary.persist_noclobber(path).map_err(|error| {
        anyhow::anyhow!(
            "publishing local dependency graph output {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::vcs::Vcs;

    #[derive(Default)]
    struct MemorySource {
        packages: BTreeMap<String, PackageMetadata>,
        candidates: BTreeMap<(String, String), Candidate>,
        candidate_failures: BTreeMap<(String, String), String>,
    }

    impl MemorySource {
        fn publish(&mut self, key: &str, version: &str, dependencies: &[(&str, &str)]) {
            let (org, name) = split_key(key).unwrap();
            self.candidates.insert(
                (key.to_string(), version.to_string()),
                Candidate {
                    key: key.to_string(),
                    version: version.to_string(),
                    scheme: VersionScheme::Semver,
                    dependencies: dependencies
                        .iter()
                        .map(|(key, requirement)| DependencySpec {
                            key: (*key).to_string(),
                            requirement: (*requirement).to_string(),
                            kind: DependencyKind::Runtime,
                        })
                        .collect(),
                    source: NodeSource::Registry,
                    artifact_sha256: Some(format!("{:064x}", self.candidates.len() + 1)),
                    yanked: false,
                },
            );
            let package = self
                .packages
                .entry(key.to_string())
                .or_insert_with(|| PackageMetadata {
                    org: org.to_string(),
                    name: name.to_string(),
                    description: None,
                    vcs: Vcs::Git,
                    repo_url: format!("https://example.invalid/{key}"),
                    version_scheme: VersionScheme::Semver,
                    latest: None,
                    tags: Vec::new(),
                    versions: Vec::new(),
                    mirrors: Vec::new(),
                    signing_keys: Vec::new(),
                });
            package.versions.push(version.to_string());
            version::sort_desc(&mut package.versions);
            package.latest = package.versions.first().cloned();
        }
    }

    impl SolveSource for MemorySource {
        fn package(&mut self, org: &str, name: &str) -> Result<PackageMetadata> {
            self.packages
                .get(&format!("{org}/{name}"))
                .cloned()
                .with_context(|| format!("missing package {org}/{name}"))
        }

        fn candidate(
            &mut self,
            key: &str,
            _org: &str,
            _name: &str,
            version: &str,
            _scheme: VersionScheme,
        ) -> Result<Candidate> {
            let cache_key = (key.to_string(), version.to_string());
            if let Some(message) = self.candidate_failures.get(&cache_key) {
                bail!("{message}");
            }
            self.candidates
                .get(&cache_key)
                .cloned()
                .with_context(|| format!("missing candidate {key}@{version}"))
        }
    }

    fn solve_memory(source: &mut MemorySource, dependencies: &[(&str, &str)]) -> Result<SolveState> {
        let root = "consumer/app@1.0.0".to_string();
        let mut state = SolveState::default();
        for (key, requirement) in dependencies {
            state.add_constraint(
                (*key).to_string(),
                Constraint {
                    requirement: (*requirement).to_string(),
                    path: vec![root.clone(), (*key).to_string()],
                    propagate: true,
                },
            )?;
        }
        let locals = BTreeMap::new();
        match (Solver {
            source,
            locals: &locals,
        })
        .solve(state)?
        {
            SearchOutcome::Solved(solved) => Ok(solved),
            SearchOutcome::Unsatisfiable(failure) => bail!(failure),
        }
    }

    #[test]
    fn diamond_graph_selects_one_shared_version() {
        let mut source = MemorySource::default();
        source.publish("test/shared", "1.0.0", &[]);
        source.publish("test/left", "1.0.0", &[("test/shared", "^1")]);
        source.publish("test/right", "1.0.0", &[("test/shared", "^1")]);
        let solved = solve_memory(
            &mut source,
            &[("test/left", "^1"), ("test/right", "^1")],
        )
        .unwrap();
        assert_eq!(solved.selected.len(), 3);
        assert_eq!(solved.selected["test/shared"].version, "1.0.0");
    }

    #[test]
    fn solver_backtracks_when_latest_candidate_conflicts() {
        let mut source = MemorySource::default();
        source.publish("test/shared", "1.0.0", &[]);
        source.publish("test/shared", "2.0.0", &[]);
        source.publish("test/router", "1.0.0", &[("test/shared", "^1")]);
        source.publish("test/router", "2.0.0", &[("test/shared", "^2")]);
        source.publish("test/policy", "1.0.0", &[("test/shared", "^1")]);
        let solved = solve_memory(
            &mut source,
            &[("test/router", ">=1"), ("test/policy", "=1.0.0")],
        )
        .unwrap();
        assert_eq!(solved.selected["test/router"].version, "1.0.0");
        assert_eq!(solved.selected["test/shared"].version, "1.0.0");
    }

    #[test]
    fn operational_candidate_errors_are_not_reinterpreted_as_version_conflicts() {
        let mut source = MemorySource::default();
        source.publish("test/router", "1.0.0", &[]);
        source.publish("test/router", "2.0.0", &[]);
        source.candidate_failures.insert(
            ("test/router".to_string(), "2.0.0".to_string()),
            "registry transport failed".to_string(),
        );

        let error = solve_memory(&mut source, &[("test/router", ">=1")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("registry transport failed"), "{error}");
        assert!(!error.contains("version conflict"), "{error}");
    }

    #[test]
    fn cycles_terminate_without_duplicate_coordinates() {
        let mut source = MemorySource::default();
        source.publish("test/a", "1.0.0", &[("test/b", "^1")]);
        source.publish("test/b", "1.0.0", &[("test/a", "^1")]);
        let solved = solve_memory(&mut source, &[("test/a", "^1")]).unwrap();
        assert_eq!(solved.selected.len(), 2);
        assert_eq!(solved.selected["test/a"].version, "1.0.0");
        assert_eq!(solved.selected["test/b"].version, "1.0.0");
    }
}
