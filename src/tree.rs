//! `zed tree` and `zed why`: read the installed dependency graph and explain it.
//!
//! Every other package manager has these two views, and a developer arriving
//! from any of them looks for them first: `npm ls` / `npm explain`,
//! `cargo tree` / `cargo tree --invert`, `pipdeptree`, `bundle viz`. This is
//! the parity surface for that habit.
//!
//! Both views are strictly offline and read-only. The lockfile pins which
//! versions are in play but records no edges — it is a flat set of immutable
//! artifact identities — so the edges come from each package's own
//! `.zpkg.toml` inside the materialization directory, the same place `npm ls`
//! reads `node_modules` from. A package the lockfile pins but that has not
//! been materialized therefore has *unknown* edges rather than *no* edges, and
//! the distinction is carried in the types instead of being flattened into an
//! empty map: a tree that silently shows a subtree as a leaf is worse than one
//! that says it cannot see that far.
//!
//! The module is split so the interesting half is pure. [`DependencySnapshot`]
//! is data; [`render`] and [`explain`] are total functions over it; only
//! [`snapshot_from_disk`] touches the filesystem.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};

/// Ceiling on the number of dependency paths `zed why` will enumerate.
///
/// A wide diamond can have combinatorially many distinct paths, and printing
/// them all is neither readable nor bounded. The cap is reported when it is
/// reached, so the output never quietly claims to be exhaustive.
const MAX_EXPLAINED_PATHS: usize = 64;

// ---------------------------------------------------------------------------
// the graph

/// A package's registry identity: `org/name`. Versions live on the node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct PackageKey {
    pub org: String,
    pub name: String,
}

impl PackageKey {
    pub fn parse(spec: &str) -> Result<Self> {
        // The version half of a spec is accepted and ignored: people type
        // `zed why acme/widget@1.2.0` out of habit from `npm explain`, and
        // refusing that is pedantry when the lock already fixes the version.
        let without_version = spec.split_once('@').map_or(spec, |(head, _)| head);
        let (org, name) = without_version
            .split_once('/')
            .ok_or_else(|| anyhow!("`{spec}` is not a package: expected `org/name`"))?;
        if org.is_empty() || name.is_empty() {
            bail!("`{spec}` is not a package: expected `org/name`");
        }
        Ok(Self {
            org: org.to_owned(),
            name: name.to_owned(),
        })
    }

    #[must_use]
    pub fn coordinate(&self) -> String {
        format!("{}/{}", self.org, self.name)
    }
}

/// What the edges out of one package are known to be.
///
/// `Unknown` is not an error and not an empty set: it is the honest state for
/// a package that is locked but not on disk, and it is rendered as such.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edges {
    Known(BTreeMap<String, Requirement>),
    Unknown,
}

/// One declared dependency edge: what was asked for, and why it exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requirement {
    pub requirement: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EdgeKind {
    Runtime,
    Build,
}

impl EdgeKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Build => "build",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNode {
    pub key: PackageKey,
    /// The version the lockfile pins, when the lockfile pins one.
    pub version: Option<String>,
    pub edges: Edges,
    /// Whether the package is present in the materialization directory.
    pub materialized: bool,
}

/// The whole readable graph: one root, and every package the lock pins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencySnapshot {
    pub root_label: String,
    pub root_edges: BTreeMap<String, Requirement>,
    pub nodes: BTreeMap<String, GraphNode>,
}

impl DependencySnapshot {
    fn node(&self, coordinate: &str) -> Option<&GraphNode> {
        self.nodes.get(coordinate)
    }

    /// Edges out of a coordinate, or out of the root when it is `None`.
    fn edges_of(&self, coordinate: Option<&str>) -> Edges {
        match coordinate {
            None => Edges::Known(self.root_edges.clone()),
            Some(coordinate) => self
                .node(coordinate)
                .map_or(Edges::Unknown, |node| node.edges.clone()),
        }
    }
}

/// Collect a package's declared edges, tagged by why each one exists.
fn edges_from_manifest(manifest: &Manifest, include_build: bool) -> BTreeMap<String, Requirement> {
    let runtime = manifest.dependencies.iter().map(|(key, requirement)| {
        (
            key.clone(),
            Requirement {
                requirement: requirement.clone(),
                kind: EdgeKind::Runtime,
            },
        )
    });
    if !include_build {
        return runtime.collect();
    }
    // A runtime edge and a build edge to the same package are one edge in the
    // tree; runtime wins the label, because that is the stronger claim.
    let build = manifest
        .build_dependencies
        .iter()
        .map(|(key, requirement)| {
            (
                key.clone(),
                Requirement {
                    requirement: requirement.clone(),
                    kind: EdgeKind::Build,
                },
            )
        });
    build.chain(runtime).collect()
}

// ---------------------------------------------------------------------------
// reading it off disk

/// Read the project's manifest, lockfile, and materialized packages.
///
/// The only impure function here. Everything it produces is plain data, so
/// every view below is testable without a filesystem.
pub fn snapshot_from_disk(project: &Path, include_build: bool) -> Result<DependencySnapshot> {
    let manifest_path = project.join(MANIFEST_FILE);
    let manifest_text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("`zed tree` needs {MANIFEST_FILE} in {}", project.display()))?;
    let manifest =
        Manifest::parse(&manifest_text).with_context(|| format!("invalid {MANIFEST_FILE}"))?;

    let modules = manifest
        .install
        .dir
        .as_deref()
        .map_or_else(|| PathBuf::from(MODULES_DIR), PathBuf::from);
    let modules_root = project.join(modules);

    let lock_path = project.join(LOCKFILE_FILE);
    let locked = match fs::read_to_string(&lock_path) {
        Ok(text) => {
            Lockfile::parse(&text)
                .with_context(|| format!("invalid {LOCKFILE_FILE}"))?
                .packages
        }
        // No lockfile is an ordinary state for a project that has never been
        // installed. The tree is still worth printing: the direct edges are in
        // the manifest, and every one of them reports as unresolved.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", lock_path.display()));
        }
    };

    let mut nodes = BTreeMap::new();
    for package in &locked {
        let key = PackageKey {
            org: package.org.clone(),
            name: package.name.clone(),
        };
        let package_dir = modules_root.join(&package.org).join(&package.name);
        let package_manifest = fs::read_to_string(package_dir.join(MANIFEST_FILE))
            .ok()
            .and_then(|text| Manifest::parse(&text).ok());
        let materialized = package_dir.is_dir();
        nodes.insert(
            key.coordinate(),
            GraphNode {
                key,
                version: Some(package.version.clone()),
                edges: match &package_manifest {
                    Some(manifest) => Edges::Known(edges_from_manifest(manifest, include_build)),
                    None => Edges::Unknown,
                },
                materialized,
            },
        );
    }

    Ok(DependencySnapshot {
        root_label: format!(
            "{}/{} {}",
            manifest.package.org, manifest.package.name, manifest.package.version
        ),
        root_edges: edges_from_manifest(&manifest, include_build),
        nodes,
    })
}

// ---------------------------------------------------------------------------
// the tree view

#[derive(Debug, Clone, Copy)]
pub struct TreeOptions {
    /// Levels below the root to print. `None` prints the whole graph.
    pub max_depth: Option<usize>,
}

/// Why a row stops where it does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RowState {
    /// Expanded normally.
    Expanded,
    /// Already shown in full elsewhere in this tree; cargo's `(*)`.
    Repeated,
    /// Reached the requested depth; there is more below.
    DepthLimited,
    /// The lockfile does not pin this requirement at all.
    Unresolved,
    /// Locked, but not on disk, so its own edges cannot be read.
    EdgesUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeRow {
    pub depth: usize,
    pub coordinate: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requirement: Option<String>,
    pub kind: EdgeKind,
    pub state: RowState,
}

/// Flatten the graph into rows, depth-first, in stable coordinate order.
///
/// Returns rows for the dependencies of `start` (the project root when
/// `None`), not including `start` itself; the caller owns how the root reads.
#[must_use]
pub fn render(
    snapshot: &DependencySnapshot,
    start: Option<&str>,
    options: TreeOptions,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let mut expanded = BTreeSet::new();
    let mut on_path = Vec::new();
    walk(
        snapshot,
        start,
        0,
        options,
        &mut expanded,
        &mut on_path,
        &mut rows,
    );
    rows
}

fn walk(
    snapshot: &DependencySnapshot,
    current: Option<&str>,
    depth: usize,
    options: TreeOptions,
    expanded: &mut BTreeSet<String>,
    on_path: &mut Vec<String>,
    rows: &mut Vec<TreeRow>,
) {
    let Edges::Known(edges) = snapshot.edges_of(current) else {
        return;
    };
    for (coordinate, requirement) in edges {
        let node = snapshot.node(&coordinate);
        let state = classify(node, &coordinate, depth, options, expanded, on_path);
        rows.push(TreeRow {
            depth,
            coordinate: coordinate.clone(),
            version: node.and_then(|node| node.version.clone()),
            requirement: Some(requirement.requirement.clone()),
            kind: requirement.kind,
            state: state.clone(),
        });
        if state != RowState::Expanded {
            continue;
        }
        expanded.insert(coordinate.clone());
        on_path.push(coordinate.clone());
        walk(
            snapshot,
            Some(&coordinate),
            depth + 1,
            options,
            expanded,
            on_path,
            rows,
        );
        on_path.pop();
    }
}

/// Decide, once, what happens to one row — so the walker has no policy in it.
fn classify(
    node: Option<&GraphNode>,
    coordinate: &str,
    depth: usize,
    options: TreeOptions,
    expanded: &BTreeSet<String>,
    on_path: &[String],
) -> RowState {
    let Some(node) = node else {
        return RowState::Unresolved;
    };
    if matches!(node.edges, Edges::Unknown) {
        return RowState::EdgesUnknown;
    }
    // A cycle is a repeat by another name, and is reported the same way: the
    // subtree has been (or is being) shown, so recursing would not terminate.
    if expanded.contains(coordinate) || on_path.iter().any(|entry| entry == coordinate) {
        return RowState::Repeated;
    }
    if options.max_depth.is_some_and(|limit| depth + 1 >= limit) {
        let has_children = matches!(&node.edges, Edges::Known(edges) if !edges.is_empty());
        return if has_children {
            RowState::DepthLimited
        } else {
            RowState::Expanded
        };
    }
    RowState::Expanded
}

/// Render rows the way a terminal wants them, with box-drawing spines.
#[must_use]
pub fn format_tree(snapshot: &DependencySnapshot, rows: &[TreeRow]) -> String {
    let mut out = String::new();
    out.push_str(&snapshot.root_label);
    out.push('\n');
    // `guides[level]` says whether the ancestor at that level still has a
    // sibling to come, which is exactly when its vertical guide is drawn.
    let mut guides: Vec<bool> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        guides.truncate(row.depth);
        let last = is_last_of_its_level(rows, index);
        for guide in guides.iter().take(row.depth) {
            out.push_str(if *guide { "\u{2502}   " } else { "    " });
        }
        out.push_str(if last {
            "\u{2514}\u{2500}\u{2500} "
        } else {
            "\u{251c}\u{2500}\u{2500} "
        });
        guides.push(!last);

        out.push_str(&row.coordinate);
        if let Some(version) = &row.version {
            out.push(' ');
            out.push_str(version);
        }
        if let Some(requirement) = &row.requirement {
            out.push_str(&format!(" ({requirement})"));
        }
        if row.kind == EdgeKind::Build {
            out.push_str(" [build]");
        }
        out.push_str(match row.state {
            RowState::Expanded => "",
            RowState::Repeated => " (*)",
            RowState::DepthLimited => " ...",
            RowState::Unresolved => "  -- not in the lockfile; run `zed install`",
            RowState::EdgesUnknown => "  -- not materialized; dependencies unknown",
        });
        out.push('\n');
    }
    out
}

/// Whether a row is the last child of its parent.
///
/// True when the next row at the same depth or shallower is shallower: a
/// shallower row means the level closed, an equal one means a sibling follows.
fn is_last_of_its_level(rows: &[TreeRow], index: usize) -> bool {
    rows[index + 1..]
        .iter()
        .find(|later| later.depth <= rows[index].depth)
        .is_none_or(|later| later.depth < rows[index].depth)
}

// ---------------------------------------------------------------------------
// the why view

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExplainedEdge {
    pub from: String,
    pub to: String,
    pub requirement: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Explanation {
    pub coordinate: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub paths: Vec<Vec<ExplainedEdge>>,
    /// True when [`MAX_EXPLAINED_PATHS`] cut the enumeration short.
    pub truncated: bool,
}

/// Every way the project reaches `target`, shortest first.
///
/// Ordering is by path length then lexicographic, so the answer to "why is
/// this here" leads with the most direct reason rather than an arbitrary one.
#[must_use]
pub fn explain(snapshot: &DependencySnapshot, target: &PackageKey) -> Explanation {
    let coordinate = target.coordinate();
    let mut paths = Vec::new();
    let mut truncated = false;
    let mut trail = Vec::new();
    let mut on_path = BTreeSet::new();
    search(
        snapshot,
        None,
        &coordinate,
        &mut trail,
        &mut on_path,
        &mut paths,
        &mut truncated,
    );
    paths.sort_by(|left, right| {
        left.len().cmp(&right.len()).then_with(|| {
            let key = |path: &Vec<ExplainedEdge>| {
                path.iter()
                    .map(|edge| edge.to.clone())
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            key(left).cmp(&key(right))
        })
    });
    Explanation {
        coordinate: coordinate.clone(),
        version: snapshot
            .node(&coordinate)
            .and_then(|node| node.version.clone()),
        paths,
        truncated,
    }
}

fn search(
    snapshot: &DependencySnapshot,
    current: Option<&str>,
    target: &str,
    trail: &mut Vec<ExplainedEdge>,
    on_path: &mut BTreeSet<String>,
    paths: &mut Vec<Vec<ExplainedEdge>>,
    truncated: &mut bool,
) {
    if paths.len() >= MAX_EXPLAINED_PATHS {
        *truncated = true;
        return;
    }
    let Edges::Known(edges) = snapshot.edges_of(current) else {
        return;
    };
    let from = current.map_or_else(|| snapshot.root_label.clone(), str::to_owned);
    for (coordinate, requirement) in edges {
        let edge = ExplainedEdge {
            from: from.clone(),
            to: coordinate.clone(),
            requirement: requirement.requirement.clone(),
            kind: requirement.kind,
        };
        if coordinate == target {
            trail.push(edge);
            paths.push(trail.clone());
            trail.pop();
            continue;
        }
        // A package already on this path cannot lead anywhere new, and
        // following it again would not terminate.
        if !on_path.insert(coordinate.clone()) {
            continue;
        }
        trail.push(edge);
        search(
            snapshot,
            Some(&coordinate),
            target,
            trail,
            on_path,
            paths,
            truncated,
        );
        trail.pop();
        on_path.remove(&coordinate);
    }
}

/// Render an explanation the way `npm explain` does: one indented chain each.
#[must_use]
pub fn format_explanation(explanation: &Explanation) -> String {
    let mut out = String::new();
    let coordinate = &explanation.coordinate;
    match &explanation.version {
        Some(version) => out.push_str(&format!("{coordinate} {version}\n")),
        None => out.push_str(&format!("{coordinate}  -- not in the lockfile\n")),
    }
    if explanation.paths.is_empty() {
        out.push_str("  nothing in this project depends on it.\n");
        return out;
    }
    for (index, path) in explanation.paths.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        for (level, edge) in path.iter().enumerate() {
            let indent = "  ".repeat(level + 1);
            let kind = if edge.kind == EdgeKind::Build {
                " [build]"
            } else {
                ""
            };
            out.push_str(&format!(
                "{indent}{} depends on {} ({}){kind}\n",
                edge.from, edge.to, edge.requirement
            ));
        }
    }
    if explanation.truncated {
        out.push_str(&format!(
            "\n  (only the first {MAX_EXPLAINED_PATHS} paths are shown)\n"
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// commands

pub fn tree(project: &Path, package: Option<&str>, depth: Option<usize>, json: bool) -> Result<()> {
    // Build dependencies are always collected: they are part of the honest
    // answer to "what does this project pull in", and the row carries the
    // label that lets a reader tell them apart.
    let snapshot = snapshot_from_disk(project, true)?;
    let start = match package {
        None => None,
        Some(spec) => {
            let key = PackageKey::parse(spec)?;
            let coordinate = key.coordinate();
            if !snapshot.nodes.contains_key(&coordinate) {
                bail!("`{coordinate}` is not in {LOCKFILE_FILE}");
            }
            Some(coordinate)
        }
    };
    let rows = render(
        &snapshot,
        start.as_deref(),
        TreeOptions { max_depth: depth },
    );
    if json {
        let document = serde_json::json!({
            "schema": "zed.tree.v1",
            "root": start.clone().unwrap_or_else(|| snapshot.root_label.clone()),
            "rows": rows,
        });
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }
    let mut snapshot = snapshot;
    if let Some(start) = &start {
        let node = snapshot.nodes.get(start).expect("checked above");
        snapshot.root_label = match &node.version {
            Some(version) => format!("{start} {version}"),
            None => start.clone(),
        };
    }
    print!("{}", format_tree(&snapshot, &rows));
    Ok(())
}

pub fn why(project: &Path, spec: &str, json: bool) -> Result<()> {
    let key = PackageKey::parse(spec)?;
    let snapshot = snapshot_from_disk(project, true)?;
    let explanation = explain(&snapshot, &key);
    if json {
        let document = serde_json::json!({
            "schema": "zed.why.v1",
            "explanation": explanation,
        });
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }
    print!("{}", format_explanation(&explanation));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(requirement: &str) -> Requirement {
        Requirement {
            requirement: requirement.to_owned(),
            kind: EdgeKind::Runtime,
        }
    }

    fn node(coordinate: &str, version: &str, edges: &[(&str, &str)]) -> (String, GraphNode) {
        let (org, name) = coordinate.split_once('/').unwrap();
        (
            coordinate.to_owned(),
            GraphNode {
                key: PackageKey {
                    org: org.to_owned(),
                    name: name.to_owned(),
                },
                version: Some(version.to_owned()),
                edges: Edges::Known(
                    edges
                        .iter()
                        .map(|(key, requirement)| ((*key).to_owned(), runtime(requirement)))
                        .collect(),
                ),
                materialized: true,
            },
        )
    }

    fn snapshot(roots: &[(&str, &str)], nodes: Vec<(String, GraphNode)>) -> DependencySnapshot {
        DependencySnapshot {
            root_label: "acme/app 1.0.0".to_owned(),
            root_edges: roots
                .iter()
                .map(|(key, requirement)| ((*key).to_owned(), runtime(requirement)))
                .collect(),
            nodes: nodes.into_iter().collect(),
        }
    }

    fn all(snapshot: &DependencySnapshot) -> Vec<TreeRow> {
        render(snapshot, None, TreeOptions { max_depth: None })
    }

    #[test]
    fn a_package_key_accepts_the_specs_people_actually_type() {
        assert_eq!(
            PackageKey::parse("acme/widget").unwrap().coordinate(),
            "acme/widget"
        );
        // `npm explain react@18` habits carry over; the version is redundant
        // here because the lock already fixes it, so it is accepted and dropped.
        assert_eq!(
            PackageKey::parse("acme/widget@1.2.0").unwrap().coordinate(),
            "acme/widget"
        );
        for rejected in ["widget", "", "/widget", "acme/"] {
            assert!(PackageKey::parse(rejected).is_err(), "{rejected}");
        }
    }

    #[test]
    fn a_project_with_no_lockfile_still_shows_what_it_asked_for() {
        // Nothing is resolved, but the direct edges are in the manifest, and
        // saying so beats printing an empty tree for a project that plainly
        // has dependencies.
        let snapshot = snapshot(&[("acme/widget", "^1")], Vec::new());
        let rows = all(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, RowState::Unresolved);
        assert_eq!(rows[0].version, None);
        assert!(format_tree(&snapshot, &rows).contains("run `zed install`"));
    }

    #[test]
    fn a_shared_dependency_is_expanded_once_and_referred_to_afterwards() {
        let snapshot = snapshot(
            &[("acme/left", "^1"), ("acme/right", "^1")],
            vec![
                node("acme/left", "1.0.0", &[("acme/shared", "^1")]),
                node("acme/right", "1.0.0", &[("acme/shared", "^1")]),
                node("acme/shared", "2.0.0", &[]),
            ],
        );
        let rows = all(&snapshot);
        let shared: Vec<_> = rows
            .iter()
            .filter(|row| row.coordinate == "acme/shared")
            .collect();
        assert_eq!(shared.len(), 2, "both parents still show the edge");
        assert_eq!(shared[0].state, RowState::Expanded);
        assert_eq!(shared[1].state, RowState::Repeated);
        assert!(format_tree(&snapshot, &rows).contains("(*)"));
    }

    #[test]
    fn a_cycle_terminates_instead_of_recursing_forever() {
        let snapshot = snapshot(
            &[("acme/a", "^1")],
            vec![
                node("acme/a", "1.0.0", &[("acme/b", "^1")]),
                node("acme/b", "1.0.0", &[("acme/a", "^1")]),
            ],
        );
        let rows = all(&snapshot);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].coordinate, "acme/a");
        assert_eq!(rows[2].state, RowState::Repeated);
    }

    #[test]
    fn a_depth_limit_says_there_is_more_below_rather_than_pretending_to_be_a_leaf() {
        let snapshot = snapshot(
            &[("acme/a", "^1")],
            vec![
                node("acme/a", "1.0.0", &[("acme/b", "^1")]),
                node("acme/b", "1.0.0", &[]),
            ],
        );
        let shallow = render(&snapshot, None, TreeOptions { max_depth: Some(1) });
        assert_eq!(shallow.len(), 1);
        assert_eq!(shallow[0].state, RowState::DepthLimited);

        // A childless package at the limit is a real leaf and must not be
        // dressed up as truncated.
        let leaf_only = self::snapshot(&[("acme/b", "^1")], vec![node("acme/b", "1.0.0", &[])]);
        let rows = render(&leaf_only, None, TreeOptions { max_depth: Some(1) });
        assert_eq!(rows[0].state, RowState::Expanded);
    }

    #[test]
    fn a_locked_but_unmaterialized_package_reports_unknown_edges_not_no_edges() {
        let mut snapshot = snapshot(
            &[("acme/widget", "^1")],
            vec![node("acme/widget", "1.0.0", &[])],
        );
        let widget = snapshot.nodes.get_mut("acme/widget").unwrap();
        widget.edges = Edges::Unknown;
        widget.materialized = false;
        let rows = all(&snapshot);
        assert_eq!(rows[0].state, RowState::EdgesUnknown);
        assert!(format_tree(&snapshot, &rows).contains("dependencies unknown"));
    }

    #[test]
    fn the_spine_closes_a_level_when_it_ends() {
        let snapshot = snapshot(
            &[("acme/first", "^1"), ("acme/second", "^1")],
            vec![
                node("acme/first", "1.0.0", &[("acme/leaf", "^1")]),
                node("acme/leaf", "1.0.0", &[]),
                node("acme/second", "1.0.0", &[]),
            ],
        );
        let rendered = format_tree(&snapshot, &all(&snapshot));
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines[0], "acme/app 1.0.0");
        assert!(
            lines[1].starts_with("\u{251c}\u{2500}\u{2500} acme/first"),
            "{rendered}"
        );
        // The nested leaf keeps its parent's guide, because a sibling follows.
        assert!(
            lines[2].starts_with("\u{2502}   \u{2514}\u{2500}\u{2500} acme/leaf"),
            "{rendered}"
        );
        assert!(
            lines[3].starts_with("\u{2514}\u{2500}\u{2500} acme/second"),
            "{rendered}"
        );
    }

    #[test]
    fn a_build_dependency_is_shown_and_labelled() {
        let mut snapshot = snapshot(&[], vec![node("acme/codegen", "1.0.0", &[])]);
        snapshot.root_edges.insert(
            "acme/codegen".to_owned(),
            Requirement {
                requirement: "^1".to_owned(),
                kind: EdgeKind::Build,
            },
        );
        let rows = all(&snapshot);
        assert_eq!(rows[0].kind, EdgeKind::Build);
        assert!(format_tree(&snapshot, &rows).contains("[build]"));
    }

    #[test]
    fn why_lists_every_path_shortest_first() {
        let snapshot = snapshot(
            &[("acme/direct", "^1"), ("acme/left", "^1")],
            vec![
                node("acme/direct", "1.0.0", &[("acme/target", "^2")]),
                node("acme/left", "1.0.0", &[("acme/middle", "^1")]),
                node("acme/middle", "1.0.0", &[("acme/target", "^2")]),
                node("acme/target", "2.1.0", &[]),
            ],
        );
        let explanation = explain(&snapshot, &PackageKey::parse("acme/target").unwrap());
        assert_eq!(explanation.version.as_deref(), Some("2.1.0"));
        assert_eq!(explanation.paths.len(), 2);
        assert_eq!(explanation.paths[0].len(), 2, "the direct route leads");
        assert_eq!(explanation.paths[1].len(), 3);
        assert!(!explanation.truncated);

        let rendered = format_explanation(&explanation);
        assert!(
            rendered.contains("acme/app 1.0.0 depends on acme/direct"),
            "{rendered}"
        );
        assert!(
            rendered.contains("acme/middle depends on acme/target (^2)"),
            "{rendered}"
        );
    }

    #[test]
    fn why_on_a_package_nothing_depends_on_says_exactly_that() {
        let snapshot = snapshot(&[], vec![node("acme/orphan", "1.0.0", &[])]);
        let explanation = explain(&snapshot, &PackageKey::parse("acme/orphan").unwrap());
        assert!(explanation.paths.is_empty());
        assert!(format_explanation(&explanation).contains("nothing in this project depends on it"));
    }

    #[test]
    fn why_terminates_on_a_cycle_that_does_not_reach_the_target() {
        let snapshot = snapshot(
            &[("acme/a", "^1")],
            vec![
                node("acme/a", "1.0.0", &[("acme/b", "^1")]),
                node("acme/b", "1.0.0", &[("acme/a", "^1")]),
            ],
        );
        let explanation = explain(&snapshot, &PackageKey::parse("acme/missing").unwrap());
        assert!(explanation.paths.is_empty());
        assert_eq!(explanation.version, None);
    }

    #[test]
    fn a_manifest_contributes_build_edges_only_when_they_are_wanted() {
        let text = r#"
[package]
org = "acme"
name = "app"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://localhost/acme/app"

[dependencies]
"acme/widget" = "^1"

[build-dependencies]
"acme/codegen" = "^2"
"#;
        let manifest = Manifest::parse(text).unwrap();
        let runtime_only = edges_from_manifest(&manifest, false);
        assert_eq!(runtime_only.len(), 1);
        assert!(runtime_only.contains_key("acme/widget"));

        let both = edges_from_manifest(&manifest, true);
        assert_eq!(both.len(), 2);
        assert_eq!(both["acme/codegen"].kind, EdgeKind::Build);
        assert_eq!(both["acme/widget"].kind, EdgeKind::Runtime);
    }

    #[test]
    fn a_package_that_is_both_a_runtime_and_a_build_dependency_is_one_runtime_edge() {
        let text = r#"
[package]
org = "acme"
name = "app"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://localhost/acme/app"

[dependencies]
"acme/shared" = "^1"

[build-dependencies]
"acme/shared" = "^1"
"#;
        let manifest = Manifest::parse(text).unwrap();
        let edges = edges_from_manifest(&manifest, true);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges["acme/shared"].kind, EdgeKind::Runtime);
    }
}
