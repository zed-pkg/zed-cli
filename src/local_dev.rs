//! Local-checkout discovery for live symlink installs.
//!
//! A normal symlink install is allowed to prefer a compatible checkout that
//! is already on the developer's machine. Frozen and copy installs never enter
//! this module, so lock replay and container materialization remain registry /
//! content-store authoritative.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result, anyhow};
use globset::Glob;
use toml::Value;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::version::VersionScheme;
use zed_lib::requirement_matches;

use crate::config::{self, Config, read_manifest};
use crate::install_graph::{self, PreparedInstall};

const MAX_DISCOVERED_DIRECTORIES: usize = 4_096;
const MAX_LOCAL_GRAPH_COORDINATES: usize = 10_000;

#[derive(Debug, Clone)]
struct LocalPackage {
    path: PathBuf,
    version: String,
    scheme: VersionScheme,
    dependencies: BTreeMap<String, String>,
}

impl LocalPackage {
    fn from_manifest(path: PathBuf, manifest: Manifest) -> Self {
        Self {
            path,
            version: manifest.package.version,
            scheme: manifest.package.version_scheme,
            dependencies: manifest.dependencies,
        }
    }

    fn satisfies_all(&self, requirements: &[String]) -> bool {
        requirements
            .iter()
            .all(|requirement| requirement_matches(self.scheme, requirement, &self.version))
    }
}

#[derive(Debug)]
struct Discovery {
    project: PathBuf,
    project_key: String,
    workspace_root: PathBuf,
    explicit: BTreeMap<String, LocalPackage>,
    index: BTreeMap<String, Vec<LocalPackage>>,
    selected: BTreeMap<String, LocalPackage>,
    all_local: bool,
}

impl Discovery {
    fn scan(project: &Path) -> Result<Self> {
        let project = normalized(project);
        let root_manifest = read_manifest(&project)?;
        let project_key = root_manifest.full_name();
        let workspace_root = effective_workspace_root(&project);
        let explicit = collect_explicit_workspace(&workspace_root);
        let index = build_local_index(&project, &workspace_root, &explicit);
        let (selected, all_local) =
            resolve_local_closure(&root_manifest.dependencies, &explicit, &index, &project_key);
        Ok(Self {
            project,
            project_key,
            workspace_root,
            explicit,
            index,
            selected,
            all_local,
        })
    }

    fn has_ambient_matches(&self) -> bool {
        !self.selected.is_empty()
    }

    /// In a mixed local/remote graph, keep the speculative remote solve as the
    /// compatibility baseline. A local checkout may replace a remote coordinate
    /// only when it satisfies the exact selected version. This avoids a local
    /// checkout changing the selected version of an otherwise remote graph.
    fn constrain_selected_to_exact(&mut self, exact: &BTreeMap<String, String>) {
        self.selected.retain(|key, package| {
            exact
                .get(key)
                .map(|requirement| package.satisfies_all(std::slice::from_ref(requirement)))
                .unwrap_or(true)
        });
    }

    /// Once the remote solver has exposed the full coordinate set, local copies
    /// of transitive dependencies can participate too. For those coordinates we
    /// require the exact selected remote version in mixed graphs.
    fn extend_from_exact_requirements(&mut self, requirements: &BTreeMap<String, String>) {
        let mut constraints: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (key, requirement) in requirements {
            if self.explicit.contains_key(key)
                || self.selected.contains_key(key)
                || key == &self.project_key
            {
                continue;
            }
            constraints
                .entry(key.clone())
                .or_default()
                .push(requirement.clone());
        }
        self.extend_selected_closure(constraints);
    }

    fn extend_selected_closure(&mut self, mut constraints: BTreeMap<String, Vec<String>>) {
        let mut queue: VecDeque<String> = constraints.keys().cloned().collect();
        let mut expanded = BTreeSet::new();
        while let Some(key) = queue.pop_front() {
            if self.explicit.contains_key(&key) || key == self.project_key {
                continue;
            }
            let Some(package) = choose_package(
                &self.index,
                &key,
                constraints.get(&key).map(Vec::as_slice).unwrap_or_default(),
            ) else {
                continue;
            };
            self.selected.insert(key.clone(), package.clone());
            if !expanded.insert((key.clone(), package.path.clone())) {
                continue;
            }
            for (dependency, requirement) in &package.dependencies {
                let list = constraints.entry(dependency.clone()).or_default();
                if !list.contains(requirement) {
                    list.push(requirement.clone());
                    queue.push_back(dependency.clone());
                }
            }
        }
    }

    fn render_workspace_overlay(&self) -> Result<Option<(PathBuf, String)>> {
        if self.selected.is_empty() {
            return Ok(None);
        }
        let manifest_path = self.workspace_root.join(MANIFEST_FILE);
        let text = fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let mut document: Value = toml::from_str(&text).with_context(|| {
            format!(
                "parsing {} for local workspace overlay",
                manifest_path.display()
            )
        })?;
        let root = document
            .as_table_mut()
            .context("package manifest root must be a TOML table")?;
        let workspace = root
            .entry("workspace".to_string())
            .or_insert_with(|| Value::Table(Default::default()))
            .as_table_mut()
            .context("[workspace] must be a TOML table")?;
        let members = workspace
            .entry("members".to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .context("workspace.members must be an array")?;

        let existing = members
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let mut additions = self
            .selected
            .values()
            .map(|package| package.path.to_string_lossy().into_owned())
            .filter(|path| !existing.contains(path))
            .collect::<Vec<_>>();
        additions.sort();
        additions.dedup();
        members.extend(additions.into_iter().map(Value::String));

        let rendered =
            toml::to_string_pretty(&document).context("serializing local workspace overlay")?;
        Ok(Some((self.workspace_root.clone(), rendered)))
    }
}

/// Race canonical remote graph preparation against bounded local checkout
/// discovery, then hand the already-prepared graph to the established install
/// facade exactly once.
///
/// When every reachable dependency is represented by a compatible explicit or
/// ambient local checkout, the remote result is abandoned and the installer
/// proceeds immediately from a local-only prepared graph. Rust's blocking HTTP
/// client cannot safely be interrupted mid-syscall, so "abandon" means we drop
/// the join handle and never wait for or consume that speculative result.
///
/// For mixed graphs, the pending remote solve is joined and reused. Ambient
/// local checkouts are then admitted only at the remote-selected exact version;
/// the ordinary workspace precedence plus `InstallMode::Symlink` turns those
/// coordinates into live links instead of store links.
pub(crate) fn with_local_dev_resolution<T>(
    project: &Path,
    cfg: &Config,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let remote_project = project.to_path_buf();
    let remote_cfg = cfg.clone();
    let remote = thread::Builder::new()
        .name("zed-local-dev-remote-resolution".to_string())
        .spawn(move || install_graph::prepare(&remote_project, &remote_cfg))
        .context("starting speculative registry resolution")?;

    let mut discovery = match Discovery::scan(project) {
        Ok(discovery) => discovery,
        Err(error) => {
            let prepared = join_remote(remote)?;
            eprintln!(
                "warning: local checkout discovery failed ({}); using registry resolution",
                error.to_string().lines().next().unwrap_or_default()
            );
            return install_graph::with_prepared_override(project, prepared, operation);
        }
    };

    let prepared = if discovery.has_ambient_matches() && discovery.all_local {
        drop(remote);
        eprintln!(
            "local-dev resolution: complete local graph found; abandoning speculative registry result"
        );
        PreparedInstall::default()
    } else {
        let prepared = join_remote(remote)?;
        let exact = prepared.exact_requirements();
        discovery.constrain_selected_to_exact(&exact);
        discovery.extend_from_exact_requirements(&exact);
        prepared
    };

    let Some((workspace_root, overlay)) = discovery.render_workspace_overlay()? else {
        return install_graph::with_prepared_override(project, prepared, operation);
    };

    eprintln!(
        "local-dev resolution: using {} compatible local checkout(s) through workspace {}",
        discovery.selected.len(),
        workspace_root.display()
    );
    config::with_manifest_override(&workspace_root, overlay, || {
        install_graph::with_prepared_override(project, prepared, operation)
    })
}

fn join_remote(remote: thread::JoinHandle<Result<PreparedInstall>>) -> Result<PreparedInstall> {
    remote
        .join()
        .map_err(|_| anyhow!("speculative registry resolution panicked"))?
}

fn normalized(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn effective_workspace_root(project: &Path) -> PathBuf {
    let mut current = Some(project);
    while let Some(directory) = current {
        if directory.join(MANIFEST_FILE).is_file()
            && let Ok(manifest) = read_manifest(directory)
            && manifest.workspace.is_some()
        {
            return normalized(directory);
        }
        current = directory.parent();
    }
    normalized(project)
}

fn collect_explicit_workspace(root: &Path) -> BTreeMap<String, LocalPackage> {
    let Ok(manifest) = read_manifest(root) else {
        return BTreeMap::new();
    };
    let Some(workspace) = manifest.workspace.as_ref() else {
        return BTreeMap::new();
    };
    let mut packages = BTreeMap::new();
    for path in expand_member_patterns(root, &workspace.members) {
        if let Ok(member) = read_manifest(&path) {
            packages.insert(
                member.full_name(),
                LocalPackage::from_manifest(normalized(&path), member),
            );
        }
    }
    packages
}

fn expand_member_patterns(root: &Path, patterns: &[String]) -> Vec<PathBuf> {
    let mut output = Vec::new();
    for pattern in patterns {
        let mut candidates = vec![root.to_path_buf()];
        for segment in pattern.split('/') {
            let mut next = Vec::new();
            for base in &candidates {
                if segment.contains('*') {
                    let Ok(glob) = Glob::new(segment) else {
                        continue;
                    };
                    let matcher = glob.compile_matcher();
                    for entry in sorted_child_dirs(base) {
                        let Some(name) = entry.file_name() else {
                            continue;
                        };
                        if matcher.is_match(Path::new(name)) {
                            next.push(entry);
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
        output.extend(candidates);
    }
    output.sort();
    output.dedup();
    output
}

fn build_local_index(
    project: &Path,
    workspace_root: &Path,
    explicit: &BTreeMap<String, LocalPackage>,
) -> BTreeMap<String, Vec<LocalPackage>> {
    let mut roots = BTreeSet::new();
    add_anchor_roots(project, &mut roots);
    add_anchor_roots(workspace_root, &mut roots);
    if let Some(home) = dirs::home_dir() {
        for name in ["codes", "code", "src", "projects", "repos", "dev"] {
            let candidate = home.join(name);
            if candidate.is_dir() {
                roots.insert(candidate);
            }
        }
    }

    let project = normalized(project);
    let explicit_paths = explicit
        .values()
        .map(|package| normalized(&package.path))
        .collect::<BTreeSet<_>>();
    let mut visited = BTreeSet::new();
    let mut index: BTreeMap<String, Vec<LocalPackage>> = BTreeMap::new();
    let mut directories = 0usize;

    'roots: for root in roots {
        if root.parent().is_none() {
            continue;
        }
        for first in sorted_child_dirs(&root) {
            directories += 1;
            if directories > MAX_DISCOVERED_DIRECTORIES {
                break 'roots;
            }
            if register_candidate(&first, &project, &explicit_paths, &mut visited, &mut index) {
                continue;
            }
            for second in sorted_child_dirs(&first) {
                directories += 1;
                if directories > MAX_DISCOVERED_DIRECTORIES {
                    break 'roots;
                }
                register_candidate(&second, &project, &explicit_paths, &mut visited, &mut index);
            }
        }
    }

    for packages in index.values_mut() {
        packages.sort_by(|left, right| {
            local_rank(&project, &left.path).cmp(&local_rank(&project, &right.path))
        });
    }
    index
}

fn add_anchor_roots(anchor: &Path, roots: &mut BTreeSet<PathBuf>) {
    if let Some(parent) = anchor.parent() {
        if parent.components().count() > 1 {
            roots.insert(parent.to_path_buf());
        }
        if let Some(grandparent) = parent.parent()
            && grandparent.components().count() > 1
        {
            roots.insert(grandparent.to_path_buf());
        }
    }
}

fn register_candidate(
    path: &Path,
    project: &Path,
    explicit_paths: &BTreeSet<PathBuf>,
    visited: &mut BTreeSet<PathBuf>,
    index: &mut BTreeMap<String, Vec<LocalPackage>>,
) -> bool {
    if !looks_like_repo(path) || !path.join(MANIFEST_FILE).is_file() {
        return false;
    }
    let path = normalized(path);
    if path == project || explicit_paths.contains(&path) || !visited.insert(path.clone()) {
        return true;
    }
    let Ok(manifest) = read_manifest(&path) else {
        return true;
    };
    index
        .entry(manifest.full_name())
        .or_default()
        .push(LocalPackage::from_manifest(path, manifest));
    true
}

fn looks_like_repo(path: &Path) -> bool {
    [".git", ".hg", ".jj"]
        .iter()
        .any(|marker| path.join(marker).exists())
}

fn sorted_child_dirs(base: &Path) -> Vec<PathBuf> {
    let mut directories = fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !ignored_directory(name))
        })
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

fn ignored_directory(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "target" | "node_modules" | "zed_modules" | "vendor" | "dist" | "build"
        )
}

fn local_rank(project: &Path, candidate: &Path) -> (u8, usize, String) {
    let same_parent = candidate.parent() == project.parent();
    let same_grandparent =
        candidate.parent().and_then(Path::parent) == project.parent().and_then(Path::parent);
    let locality = if same_parent {
        0
    } else if same_grandparent {
        1
    } else {
        2
    };
    (
        locality,
        candidate.components().count(),
        candidate.to_string_lossy().into_owned(),
    )
}

fn choose_package(
    index: &BTreeMap<String, Vec<LocalPackage>>,
    key: &str,
    requirements: &[String],
) -> Option<LocalPackage> {
    index
        .get(key)?
        .iter()
        .find(|package| package.satisfies_all(requirements))
        .cloned()
}

fn resolve_local_closure(
    root_dependencies: &BTreeMap<String, String>,
    explicit: &BTreeMap<String, LocalPackage>,
    index: &BTreeMap<String, Vec<LocalPackage>>,
    project_key: &str,
) -> (BTreeMap<String, LocalPackage>, bool) {
    let mut constraints: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut queue = VecDeque::new();
    for (key, requirement) in root_dependencies {
        constraints
            .entry(key.clone())
            .or_default()
            .push(requirement.clone());
        queue.push_back(key.clone());
    }

    let mut selected = BTreeMap::new();
    let mut expanded: BTreeSet<(String, PathBuf)> = BTreeSet::new();
    let mut all_local = true;
    while let Some(key) = queue.pop_front() {
        if constraints.len() > MAX_LOCAL_GRAPH_COORDINATES {
            all_local = false;
            break;
        }
        if key == project_key {
            continue;
        }
        let requirements = constraints.get(&key).cloned().unwrap_or_default();
        let package = explicit
            .get(&key)
            .filter(|package| package.satisfies_all(&requirements))
            .cloned()
            .or_else(|| choose_package(index, &key, &requirements));
        let Some(package) = package else {
            selected.remove(&key);
            all_local = false;
            continue;
        };

        if !explicit.contains_key(&key) {
            selected.insert(key.clone(), package.clone());
        }
        if !expanded.insert((key.clone(), package.path.clone())) {
            continue;
        }
        for (dependency, requirement) in &package.dependencies {
            let list = constraints.entry(dependency.clone()).or_default();
            if !list.contains(requirement) {
                list.push(requirement.clone());
                queue.push_back(dependency.clone());
            }
        }
    }
    (selected, all_local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture_root() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("zed-local-dev-{unique}-{}", std::process::id()))
    }

    fn write_manifest(path: &Path, org: &str, name: &str, version: &str, deps: &[(&str, &str)]) {
        fs::create_dir_all(path).unwrap();
        let mut text = format!(
            "[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"fixture\"\nlicense = \"MIT\"\nlanguage = \"rust\"\n"
        );
        if !deps.is_empty() {
            text.push_str("\n[dependencies]\n");
            for (key, requirement) in deps {
                text.push_str(&format!("\"{key}\" = \"{requirement}\"\n"));
            }
        }
        fs::write(path.join(MANIFEST_FILE), text).unwrap();
    }

    #[test]
    fn discovers_compatible_cousin_repo_and_ignores_wrong_version() {
        let root = fixture_root();
        let app = root.join("codes/app-org/app");
        let good = root.join("codes/lib-org/lib");
        let wrong = root.join("codes/aaa-shadow/lib");
        write_manifest(&app, "app-org", "app", "1.0.0", &[("lib-org/lib", "^1")]);
        write_manifest(&good, "lib-org", "lib", "1.2.0", &[]);
        write_manifest(&wrong, "lib-org", "lib", "2.0.0", &[]);
        fs::create_dir_all(good.join(".git")).unwrap();
        fs::create_dir_all(wrong.join(".git")).unwrap();

        let discovery = Discovery::scan(&app).unwrap();
        let selected = discovery.selected.get("lib-org/lib").unwrap();
        assert_eq!(selected.version, "1.2.0");
        assert!(discovery.all_local);
        assert_eq!(selected.path, normalized(&good));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn local_miss_keeps_remote_resolution_required() {
        let root = fixture_root();
        let app = root.join("codes/app-org/app");
        write_manifest(&app, "app-org", "app", "1.0.0", &[("missing/lib", "^1")]);
        let discovery = Discovery::scan(&app).unwrap();
        assert!(discovery.selected.is_empty());
        assert!(!discovery.all_local);
        let _ = fs::remove_dir_all(root);
    }
}
