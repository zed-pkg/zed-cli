use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::BufRead;
#[cfg(windows)]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use zed_interfaces::language::{Ecosystem, Language, detect_ecosystems};
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::manifest::{
    BuildSection, InstallHooksSection, Manifest, NativeDependencies, PackageSection,
    PublishSection, RepositorySection, ScriptsSection, is_slug,
};
use zed_interfaces::paths::{BIN_DIR, LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR, current_platform};
use zed_interfaces::registry::{PublishMeta, VersionMetadata};
use zed_interfaces::vcs::Vcs;
use zed_interfaces::version::{self, Requirement};

use crate::cli::{Adapter, InstallMode};
use crate::config::{Config, Credentials, read_manifest, write_manifest};
use crate::interactive;
use crate::native::{self, NativeInstallOutcome, NativeRequirement};
use crate::pack::{self, PackResult};
use crate::registry::{Registry, registry_for};
use crate::store::{Store, human_size, require_sha256};
use crate::transaction::ProjectTransaction;
use crate::vcs::verify_publish_provenance;

pub fn split_key(key: &str) -> Result<(String, String)> {
    let mut parts = key.splitn(2, '/');
    match (parts.next(), parts.next()) {
        (Some(org), Some(name)) if !org.is_empty() && !name.is_empty() => {
            Ok((org.to_string(), name.to_string()))
        }
        _ => bail!("invalid package spec `{key}` (expected org/name)"),
    }
}

/// Registry responses feed org/name/sha into filesystem paths (store
/// entries, zed_modules links, node_modules links). A malicious or
/// compromised registry must not be able to point those outside their
/// intended directories, so every `VersionMetadata` is validated at the
/// trust boundary before any disk operation uses it.
fn validate_version_metadata(vm: &VersionMetadata) -> Result<()> {
    if !is_slug(&vm.org) || !is_slug(&vm.name) {
        bail!(
            "registry returned invalid package identity `{}/{}`; refusing",
            vm.org,
            vm.name
        );
    }
    require_sha256(&vm.sha256)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// init

pub fn init(
    dir: &Path,
    org: Option<String>,
    name: Option<String>,
    interactive_mode: bool,
) -> Result<()> {
    let manifest_path = dir.join(MANIFEST_FILE);
    if manifest_path.exists() {
        bail!("{MANIFEST_FILE} already exists in {}", dir.display());
    }
    let name = name.unwrap_or_else(|| {
        dir.file_name()
            .map(|n| n.to_string_lossy().to_lowercase().replace(['_', ' '], "-"))
            .unwrap_or_else(|| "my-package".to_string())
    });
    let org = org.unwrap_or_else(|| "your-org".to_string());
    interactive::confirm(
        interactive_mode,
        &format!(
            "create {MANIFEST_FILE} for {org}/{name} in {}",
            dir.display()
        ),
    )?;
    let repo_url = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| format!("https://github.com/{org}/{name}"));

    let template = format!(
        r#"[package]
org = "{org}"
name = "{name}"
version = "0.1.0"
description = ""
license = "MIT"

# The backing repo: any git/hg/jj/sapling/fossil/pijul host (GitHub, GitLab,
# Bitbucket, Codeberg, SourceHut, Forgejo, Gitea, or self-hosted). A matching
# tag (publish.tag_format) must exist there before `zed publish`.
[package.repository]
vcs = "git"
url = "{repo_url}"

[dependencies]
# "acme/http-kit" = "^1"

[publish]
# Extra globs to strip beyond the defaults (tests, CI, .github, READMEs):
exclude = []
# Run by `zed r2g` inside a throwaway consumer project (optionally in a
# container) that has this package installed the way a real consumer would:
# smoke_test = "test -f \"$ZED_PKG_TEST_TARGET/.zpkg.toml\""

[scripts]
# test = "make test"
"#
    );
    fs::write(&manifest_path, template)?;

    let gitignore = dir.join(".gitignore");
    let ignore_lines = [
        format!("{MODULES_DIR}/"),
        ".zed/*".to_string(),
        "!.zed/environment.lock.toml".to_string(),
        format!("{}/", crate::transaction::STAGING_DIR),
    ];
    if gitignore.exists() {
        let mut current = fs::read_to_string(&gitignore)?;
        for ignore in &ignore_lines {
            if !current.lines().any(|line| line.trim() == ignore) {
                if !current.is_empty() && !current.ends_with('\n') {
                    current.push('\n');
                }
                current.push_str(ignore);
                current.push('\n');
            }
        }
        fs::write(&gitignore, current)?;
    } else {
        fs::write(&gitignore, ignore_lines.join("\n") + "\n")?;
    }
    println!("wrote {MANIFEST_FILE} for {org}/{name}");
    Ok(())
}

// ---------------------------------------------------------------------------
// workspaces

/// A discovered monorepo workspace: the root manifest's `[workspace]`
/// member globs expanded to actual member packages.
#[derive(Debug, Default)]
pub struct WorkspaceInfo {
    pub root: PathBuf,
    /// `org/name` -> member source directory.
    pub members: BTreeMap<String, PathBuf>,
}

/// Walk up from `project` looking for a manifest with a `[workspace]`
/// section; expand its member globs into packages. Members are linked from
/// source instead of the registry so edits show up in consumers instantly.
fn find_workspace(project: &Path) -> Option<WorkspaceInfo> {
    let mut dir: Option<&Path> = Some(project);
    while let Some(d) = dir {
        if d.join(MANIFEST_FILE).exists()
            && let Ok(manifest) = read_manifest(d)
            && let Some(ws) = &manifest.workspace
        {
            return Some(collect_members(d, &ws.members));
        }
        dir = d.parent();
    }
    None
}

fn collect_members(root: &Path, globs: &[String]) -> WorkspaceInfo {
    let members = globs.iter().flat_map(|pattern| {
        // Member globs are directory patterns like `packages/*`; expand one
        // path segment at a time so we never walk unrelated trees.
        pattern
            .split('/')
            .fold(vec![root.to_path_buf()], expand_glob_segment)
            .into_iter()
            .filter_map(|member_dir| {
                read_manifest(&member_dir)
                    .ok()
                    .map(|member| (member.full_name(), member_dir))
            })
    });
    WorkspaceInfo {
        root: root.to_path_buf(),
        members: members.collect(),
    }
}

fn expand_glob_segment(candidates: Vec<PathBuf>, segment: &str) -> Vec<PathBuf> {
    candidates
        .into_iter()
        .flat_map(|base| {
            if segment.contains('*') {
                let Ok(glob) = globset::Glob::new(segment) else {
                    return Vec::new();
                };
                let matcher = glob.compile_matcher();
                fs::read_dir(&base)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|entry| {
                        let name = entry.file_name();
                        entry.path().is_dir()
                            && matcher.is_match(Path::new(&name))
                            && !name.to_string_lossy().starts_with('.')
                    })
                    .map(|entry| entry.path())
                    .collect()
            } else {
                let candidate = base.join(segment);
                candidate.is_dir().then_some(candidate).into_iter().collect()
            }
        })
        .collect()
}

/// A dependency on the current package identity is an explicit request to
/// install and test its published artifact. Linking the workspace member back
/// into its own dependency directory would silently test source instead and
/// can create source/destination recursion. Other workspace dependencies,
/// including ordinary cross-package cycles, keep source-link semantics.
fn workspace_member_for_dependency<'a>(
    manifest: &Manifest,
    workspace: Option<&'a WorkspaceInfo>,
    key: &str,
) -> Option<&'a PathBuf> {
    if key == manifest.full_name() {
        return None;
    }
    workspace?.members.get(key)
}

fn collect_workspace_links_for_frozen(
    project: &Path,
    manifest: &Manifest,
    workspace: Option<&WorkspaceInfo>,
) -> Result<BTreeMap<String, PathBuf>> {
    let Some(workspace) = workspace else {
        return Ok(BTreeMap::new());
    };

    let mut links = BTreeMap::new();
    let mut pending: VecDeque<(String, String)> = manifest
        .dependencies
        .iter()
        .map(|(key, requirement)| (key.clone(), requirement.clone()))
        .collect();

    while let Some((raw_key, requirement_text)) = pending.pop_front() {
        let (org, name) = split_key(&raw_key)?;
        let key = format!("{org}/{name}");
        let Some(member_dir) = workspace_member_for_dependency(manifest, Some(workspace), &key)
        else {
            continue;
        };
        let member_manifest = read_manifest(member_dir).with_context(|| {
            format!(
                "reading workspace member `{key}` from {}",
                member_dir.display()
            )
        })?;
        let requirement = Requirement::parse(&requirement_text);
        if !requirement.matches(&member_manifest.package.version) {
            bail!(
                "workspace member {key}@{} does not satisfy `{requirement_text}`",
                member_manifest.package.version
            );
        }
        if member_dir == project || links.contains_key(&key) {
            continue;
        }
        links.insert(key, member_dir.clone());
        pending.extend(member_manifest.dependencies);
    }

    Ok(links)
}

// ---------------------------------------------------------------------------
// install

#[derive(Debug)]
pub struct InstallOutcome {
    pub installed: Vec<(String, String)>,
}

/// Independent trust decisions for package-authored lifecycle behavior.
/// Existing library callers that use [`install`] retain the historical
/// build-only opt-in while the CLI uses [`install_with_permissions`] to pass
/// native-dependency and install-hook consent explicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallPermissions {
    pub allow_build: bool,
    pub allow_native_deps: bool,
    pub allow_install_hooks: bool,
    pub native_manager: Option<String>,
}

#[derive(Debug, Clone)]
struct PackageSource {
    dir: PathBuf,
    manifest: Option<Manifest>,
}

fn projected_root_target<'a>(manifest: &Manifest, requested: Option<&'a str>) -> Option<&'a str> {
    requested.filter(|target| {
        manifest.targets.is_empty() || manifest.resolve_target_key(target).is_some()
    })
}

fn package_install_metadata(
    manifest: Option<&Manifest>,
    target: Option<&str>,
) -> Result<(NativeDependencies, InstallHooksSection)> {
    let Some(manifest) = manifest else {
        return Ok((NativeDependencies::new(), InstallHooksSection::default()));
    };
    Ok((
        manifest.effective_native_dependencies(target)?,
        manifest.effective_install_hooks(target)?,
    ))
}

fn read_artifact_manifest(dir: &Path, key: &str) -> Result<Manifest> {
    read_manifest(dir).with_context(|| {
        format!(
            "artifact for `{key}` is missing a valid {MANIFEST_FILE} at {}",
            dir.display()
        )
    })
}

fn ensure_artifact(reg: &dyn Registry, store: &Store, vm: &VersionMetadata) -> Result<PathBuf> {
    validate_version_metadata(vm)?;
    crate::install_graph::ensure_artifact(reg, store, vm)
        .map(|(package_dir, _downloaded)| package_dir)
}

#[cfg(test)]
pub(crate) fn legacy_ensure_artifact_for_test(
    reg: &dyn Registry,
    store: &Store,
    vm: &VersionMetadata,
) -> Result<PathBuf> {
    ensure_artifact(reg, store, vm)
}

/// Resolve the requested mode once, before any project output is written.
/// Windows cannot create Zed's Unix store-backed directory links reliably;
/// report that platform decision and use the portable ownership model.
fn replace_dest(destination: &Path) -> Result<()> {
    crate::materialize::replace_destination(destination)
}

fn copy_dir(source: &Path, destination: &Path) -> Result<()> {
    crate::materialize::copy_directory(source, destination)
}

fn effective_install_mode(mode: InstallMode) -> InstallMode {
    #[cfg(unix)]
    {
        mode
    }
    #[cfg(not(unix))]
    {
        match mode {
            InstallMode::Symlink => {
                eprintln!(
                    "warning: symlink install mode is unavailable on this platform; using copy mode"
                );
                InstallMode::Copy
            }
            InstallMode::Copy => InstallMode::Copy,
        }
    }
}

fn link_or_copy(src: &Path, dest: &Path, mode: InstallMode) -> Result<()> {
    crate::materialize::link_or_copy(src, dest, mode)
}

/// Pick the language subtree to take from *polyglot* dependencies — a repo
/// that ships the same library for several ecosystems under e.g. `node/`,
/// `python/`, `go/`. Precedence, most explicit first:
///
///   1. `--target` / `ZED_PKG_TARGET`
///   2. `[install].target` in the consumer's manifest
///   3. inference from the project's own native manifest
///
/// Inference is what makes `zed install zedtest/polyglot-lib` do the right
/// thing in each consumer without any extra configuration: the same command
/// in a Node app and a Python app materializes different source folders.
/// Returns `None` when nothing indicates a language, which installs the whole
/// tree (the pre-polyglot behavior).
fn resolve_target(project: &Path, manifest: &Manifest, flag: Option<&str>) -> Option<String> {
    if let Some(explicit) = flag.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(explicit.to_string());
    }
    if let Some(configured) = manifest.requested_target() {
        return Some(configured.to_string());
    }
    detect_target(project)
}

/// Infer the ecosystem from the files a project keeps at its root. A native
/// package-manager manifest is authoritative; source-layout inference is only a
/// fallback for intentionally pre-manifest project skeletons.
pub(crate) fn detect_target(project: &Path) -> Option<String> {
    detect_native_manifest_target(project).or_else(|| detect_structure_target(project))
}

/// Detect only authoritative native manifests. Manifestless root selection uses
/// this separately so a nearer `main.go` or `src/main.rs` cannot outrank the
/// actual `go.mod` or `Cargo.toml` that owns the project.
pub(crate) fn detect_native_manifest_target(project: &Path) -> Option<String> {
    const MARKERS: &[(&str, &str)] = &[
        ("package.json", "node"),
        ("tsconfig.json", "node"),
        ("Cargo.toml", "rust"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("pubspec.yaml", "dart"),
        ("mix.exs", "elixir"),
        ("rebar.config", "erlang"),
        ("gleam.toml", "gleam"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("build.gradle.kts", "java"),
        ("Gemfile", "ruby"),
        ("composer.json", "php"),
        ("Package.swift", "swift"),
        ("shard.yml", "crystal"),
        ("dune-project", "ocaml"),
        ("build.zig.zon", "zig"),
        ("DESCRIPTION", "r"),
        // Julia's Project.toml is checked after the more specific markers
        // above so a repo carrying both is not mistaken for Julia.
        ("Project.toml", "julia"),
        ("CMakeLists.txt", "cpp"),
    ];
    MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}

/// Detect bounded source layouts for projects that intentionally do not yet
/// have their ecosystem manifest. This is weaker evidence than a native
/// manifest and must never move an install below an authoritative ancestor.
pub(crate) fn detect_structure_target(project: &Path) -> Option<String> {
    const STRUCTURE_MARKERS: &[(&str, &str)] = &[
        ("src/main.rs", "rust"),
        ("src/lib.rs", "rust"),
        ("src/index.ts", "node"),
        ("src/main.ts", "node"),
        ("src/index.js", "node"),
        ("src/main.js", "node"),
        ("main.go", "go"),
        ("cmd/main.go", "go"),
        ("main.py", "python"),
        ("app.py", "python"),
        ("src/main.py", "python"),
        ("lib/main.dart", "dart"),
        ("src/main.gleam", "gleam"),
        ("src/main/java", "java"),
        ("src/main/kotlin", "java"),
    ];
    STRUCTURE_MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}

/// Pick the ecosystem adapter from the same language inference used for target
/// slicing so marker-only and structure-only consumer folders agree.
///
/// Deriving from `detect_target` rather than a second marker table is what keeps
/// the two from drifting: a project that resolves the `golang` slice of a
/// dependency necessarily gets the Go adapter that wires it.
pub(crate) fn detect_adapter(project: &Path) -> Adapter {
    match detect_target(project).as_deref() {
        Some("node") => Adapter::Node,
        Some("java") => Adapter::Java,
        Some("go") => Adapter::Go,
        Some("python") => Adapter::Python,
        Some("rust") => Adapter::Rust,
        Some("dart") => Adapter::Dart,
        // Every other language installs into zed_modules/ and is described in
        // .zed/paths.json; only these six have native wiring zed can emit.
        _ => Adapter::None,
    }
}

fn named_adapter(value: &str) -> Result<Adapter> {
    match value {
        "node" => Ok(Adapter::Node),
        "java" => Ok(Adapter::Java),
        "go" => Ok(Adapter::Go),
        "python" => Ok(Adapter::Python),
        "rust" => Ok(Adapter::Rust),
        "dart" => Ok(Adapter::Dart),
        "none" => Ok(Adapter::None),
        other => bail!(
            "unsupported install adapter `{other}`; expected one of {}",
            zed_interfaces::manifest::ADAPTERS.join(", ")
        ),
    }
}

#[cfg(test)]
#[test]
fn every_manifest_adapter_name_maps_to_an_adapter() {
    // `ADAPTERS` in zed-interfaces is what validates manifests; this mapping is
    // what acts on them. If they drift, a manifest passes validation and then
    // fails at install with "unsupported adapter".
    for name in zed_interfaces::manifest::ADAPTERS {
        assert!(
            named_adapter(name).is_ok(),
            "manifest accepts adapter `{name}` but the CLI cannot map it"
        );
    }
}

/// Every ecosystem this project's own root files identify. A set, because
/// polyglot repos are normal: a Rust service with a TypeScript frontend is both
/// `cargo` and `npm`, and a dependency for either legitimately belongs there.
///
/// An unreadable directory yields an empty set, which
/// [`ecosystem_mismatch`] treats as "cannot verify" rather than "wrong".
fn project_ecosystems(project: &Path) -> BTreeSet<Ecosystem> {
    let Ok(entries) = fs::read_dir(project) else {
        return BTreeSet::new();
    };
    let names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    detect_ecosystems(names.iter().map(String::as_str))
}

/// Names of sibling packages that would suit this project, for a wrong-language
/// install. Derived locally from the naming convention — `<base>-<language>` —
/// by swapping the dependency's language suffix for one matching an ecosystem
/// the project actually has.
///
/// These are suggestions, not registry lookups: the message says "try", because
/// a repo need not publish every language.
fn sibling_suggestions(
    dep_name: &str,
    dep_language: Language,
    wanted: &BTreeSet<Ecosystem>,
) -> Vec<String> {
    let suffix = format!("-{}", dep_language.as_str());
    let Some(base) = dep_name.strip_suffix(&suffix) else {
        return Vec::new();
    };
    let mut names: Vec<String> = LANGUAGES_BY_ECOSYSTEM
        .iter()
        .filter(|(eco, _)| wanted.contains(eco))
        .map(|(_, lang)| format!("{base}-{lang}"))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The languages worth suggesting per ecosystem — the ones these client repos
/// actually publish. Deliberately not every [`Language`]: suggesting `-clojure`
/// to a Gradle user because Clojure is a JVM language would be noise.
const LANGUAGES_BY_ECOSYSTEM: &[(Ecosystem, &str)] = &[
    (Ecosystem::Npm, "nodejs"),
    (Ecosystem::Jvm, "java"),
    (Ecosystem::Jvm, "kotlin"),
    (Ecosystem::Gomod, "golang"),
    (Ecosystem::Pypi, "python"),
    (Ecosystem::Cargo, "rust"),
    (Ecosystem::Pub, "dart"),
    (Ecosystem::Gem, "ruby"),
    (Ecosystem::Composer, "php"),
    (Ecosystem::Nuget, "csharp"),
    (Ecosystem::Swiftpm, "swift"),
    (Ecosystem::Hex, "gleam"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct GoDirectiveVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

fn parse_go_directive(text: &str) -> Option<(GoDirectiveVersion, String)> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("go") {
            continue;
        }
        let token = fields.next()?;
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next().map(str::parse).transpose().ok()?.unwrap_or(0);
        if parts.next().is_some() {
            return None;
        }
        return Some((
            GoDirectiveVersion {
                major,
                minor,
                patch,
            },
            token.to_string(),
        ));
    }
    None
}

/// A go.work file must declare a Go version at least as new as every module it
/// includes. Start from Zed's compatibility floor and select the highest
/// numeric `go` directive from the consumer and installed package modules.
fn required_go_work_version(project: &Path, paths: &[PathBuf]) -> String {
    let mut selected = (
        GoDirectiveVersion {
            major: 1,
            minor: 21,
            patch: 0,
        },
        "1.21".to_string(),
    );
    for root in std::iter::once(project).chain(paths.iter().map(PathBuf::as_path)) {
        let Ok(document) = fs::read_to_string(root.join("go.mod")) else {
            continue;
        };
        let Some(candidate) = parse_go_directive(&document) else {
            continue;
        };
        if candidate.0 > selected.0 {
            selected = candidate;
        }
    }
    selected.1
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CargoPatchEntry {
    package: String,
    config_path: String,
}

fn cargo_package_name(root: &Path) -> Result<Option<String>> {
    let manifest_path = root.join("Cargo.toml");
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let document = fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let document: toml::Value = toml::from_str(&document)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    let Some(package) = document.get("package") else {
        return Ok(None);
    };
    let package = package
        .as_table()
        .with_context(|| format!("{}: [package] must be a table", manifest_path.display()))?;
    let name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .with_context(|| format!("{}: [package].name is required", manifest_path.display()))?;
    if name.trim().is_empty() {
        bail!(
            "{}: [package].name must not be empty",
            manifest_path.display()
        );
    }
    Ok(Some(name.to_string()))
}

fn cargo_patch_entries(project: &Path, paths: &[PathBuf]) -> Result<Vec<CargoPatchEntry>> {
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    for path in paths {
        let Some(package) = cargo_package_name(path)? else {
            eprintln!(
                "warning: {} has no root [package].name; emitting a Cargo paths override without a crates.io patch entry",
                path.display()
            );
            continue;
        };
        let config_path = relative_to(project, path);
        if let Some(existing) = entries.get(&package)
            && existing != &config_path
        {
            bail!(
                "installed Rust crates `{}` and `{}` both declare package name `{package}`; Cargo cannot patch one crate name to two paths",
                existing,
                config_path
            );
        }
        entries.insert(package, config_path);
    }
    Ok(entries
        .into_iter()
        .map(|(package, config_path)| CargoPatchEntry {
            package,
            config_path,
        })
        .collect())
}

fn toml_basic_string(value: &str) -> Result<String> {
    serde_json::to_string(value).context("quoting a generated Cargo configuration string")
}

/// Emit the native wiring file for each adapter that needs one.
///
/// Go and Python get zero-touch integration: both honor an environment variable
/// (`GOWORK`, `PYTHONPATH`), so pointing the toolchain at the generated file is
/// the whole setup. Cargo and pub have **no** such override — nothing outside
/// `Cargo.toml` / `pubspec.yaml` can add a dependency path — so those two get a
/// fragment plus the one line to paste. Saying so is better than emitting a
/// file that silently does nothing.
fn write_toolchain_wiring(project: &Path, roots: &BTreeMap<Adapter, Vec<PathBuf>>) -> Result<()> {
    let zed_dir = project.join(".zed");
    for (adapter, paths) in roots {
        if paths.is_empty() {
            continue;
        }
        let mut rel: Vec<String> = paths.iter().map(|p| relative_to(project, p)).collect();
        rel.sort();
        rel.dedup();
        fs::create_dir_all(&zed_dir)?;
        match adapter {
            Adapter::Go => {
                // A go.work `use` block is the only non-invasive way to add
                // modules to a Go build; editing go.mod `replace` lines would
                // mean rewriting a file the user owns. Go resolves every path
                // relative to the go.work file itself, which lives in `.zed/`,
                // not relative to the process working directory.
                let mut work_paths: Vec<String> = std::iter::once(project)
                    .chain(paths.iter().map(PathBuf::as_path))
                    .map(|path| {
                        pathdiff_relative(&zed_dir, path)
                            .to_string_lossy()
                            .replace('\\', "/")
                    })
                    .collect();
                work_paths.sort();
                work_paths.dedup();
                let version = required_go_work_version(project, paths);
                let mut doc = format!("go {version}\n\nuse (\n");
                for path in &work_paths {
                    doc.push_str(&format!("\t{path}\n"));
                }
                doc.push_str(")\n");
                let path = zed_dir.join("go.work");
                fs::write(&path, doc)?;
                println!(
                    "wrote {} ({} module(s)); use: GOWORK=\"$(pwd)/.zed/go.work\" go build ...",
                    path.display(),
                    rel.len()
                );
            }
            Adapter::Python => {
                let path = zed_dir.join("pythonpath");
                fs::write(&path, format!("{}\n", rel.join(":")))?;
                println!(
                    "wrote {} ({} path(s)); use: PYTHONPATH=\"$(cat .zed/pythonpath)\" python ...",
                    path.display(),
                    rel.len()
                );
            }
            Adapter::Rust => {
                // Cargo's `paths` override can replace a package that already
                // participates in resolution, but it cannot introduce an
                // unpublished crate by itself. Pair it with config-level
                // `[patch.crates-io]` entries so a normal version dependency can
                // resolve to the installed Zed crate without touching the
                // consumer's Cargo.toml.
                let mut config_paths: Vec<String> = paths
                    .iter()
                    .map(|path| relative_to(project, path))
                    .collect();
                config_paths.sort();
                config_paths.dedup();
                let patches = cargo_patch_entries(project, paths)?;

                let mut doc = String::from(
                    "# Generated by `zed install` for this project root.\n\
                     # Copy or merge this fragment into .cargo/config.toml.\n\
                     # The consumer's Cargo.toml remains unchanged.\n",
                );
                doc.push_str("paths = [\n");
                for path in &config_paths {
                    doc.push_str(&format!("    {},\n", toml_basic_string(path)?));
                }
                doc.push_str("]\n");
                if !patches.is_empty() {
                    doc.push_str("\n[patch.crates-io]\n");
                    for patch in &patches {
                        doc.push_str(&format!(
                            "{} = {{ path = {} }}\n",
                            toml_basic_string(&patch.package)?,
                            toml_basic_string(&patch.config_path)?,
                        ));
                    }
                }
                let path = zed_dir.join("cargo-paths.toml");
                fs::write(&path, doc)?;
                println!(
                    "wrote {} ({} crate(s)); copy or merge into .cargo/config.toml",
                    path.display(),
                    config_paths.len()
                );
            }
            Adapter::Dart => {
                let mut doc = String::from(
                    "# Generated by `zed install`. pub has no environment-variable\n\
                     # path override; merge these entries under `dependencies:` in\n\
                     # your pubspec.yaml.\n",
                );
                for p in &rel {
                    let name = Path::new(p)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| p.clone());
                    doc.push_str(&format!("{name}:\n  path: {p}\n"));
                }
                let path = zed_dir.join("pub-deps.yaml");
                fs::write(&path, doc)?;
                println!(
                    "wrote {} ({} package(s)); pub needs this merged into pubspec.yaml manually",
                    path.display(),
                    rel.len()
                );
            }
            Adapter::Auto | Adapter::None | Adapter::Node | Adapter::Java => {}
        }
    }
    Ok(())
}

/// One installed package, as recorded in `.zed/paths.json`.
struct WiredPackage {
    key: String,
    version: String,
    language: Language,
    ecosystem: Ecosystem,
    path: PathBuf,
}

/// Write `.zed/paths.json`: every installed package with its language,
/// ecosystem and project-relative path.
///
/// This is the adapter-independent contract. zed federates ~30 ecosystems but
/// can only ship first-class wiring for a handful, so rather than leave the
/// rest with nothing, every install emits one machine-readable index any build
/// system — Makefile, CMake, sbt, a shell script — can read to find what was
/// installed. Written for *all* adapters, including node and java, so tooling
/// never has to care which one ran.
fn write_paths_index(project: &Path, modules_dir: &str, packages: &[WiredPackage]) -> Result<()> {
    let entries: Vec<serde_json::Value> = packages
        .iter()
        .map(|p| {
            serde_json::json!({
                "package": p.key,
                "version": p.version,
                "language": p.language.as_str(),
                "ecosystem": p.ecosystem.as_str(),
                "path": relative_to(project, &p.path),
            })
        })
        .collect();
    let doc = serde_json::json!({
        "modules_dir": modules_dir,
        "packages": entries,
    });
    let path = project.join(".zed").join("paths.json");
    fs::create_dir_all(path.parent().context("paths.json parent")?)?;
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&doc)?))?;
    Ok(())
}

/// `path` expressed relative to `project` when it sits underneath it, else
/// absolute. Keeps the emitted wiring files portable across the symlink/copy
/// modes and usable from inside a container build.
fn relative_to(project: &Path, path: &Path) -> String {
    path.strip_prefix(project)
        .map(|rel| rel.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

/// The error text for installing a single-language package into a project that
/// cannot consume it — `None` when the install is fine.
///
/// This is the guard that makes per-language packages safe: without it a Node
/// project can `zed add acme/acme-clients-java` and get a tree of `.java` files
/// its toolchain will never look at, with no complaint at any point.
///
/// Deliberately permissive in two cases, because a false rejection is worse
/// than a missed catch:
///   * the dependency claims no ecosystem (`universal`) — nothing to contradict;
///   * the project has no recognizable ecosystem at all (a fresh dir, a plain
///     Makefile) — unverifiable, so it is allowed through.
fn ecosystem_mismatch(
    dep_key: &str,
    dep_name: &str,
    dep_language: Language,
    dep_ecosystem: Ecosystem,
    project: &BTreeSet<Ecosystem>,
) -> Option<String> {
    if dep_ecosystem.is_default() || project.is_empty() || project.contains(&dep_ecosystem) {
        return None;
    }
    let found: Vec<&str> = project.iter().map(|e| e.as_str()).collect();
    let mut message = format!(
        "`{dep_key}` targets the `{dep_ecosystem}` ecosystem, but this project looks like `{}`",
        found.join("`, `")
    );
    let suggestions = sibling_suggestions(dep_name, dep_language, project);
    if !suggestions.is_empty() {
        let (org, _) = dep_key.split_once('/').unwrap_or(("", dep_key));
        let listed: Vec<String> = suggestions
            .iter()
            .map(|name| format!("{org}/{name}"))
            .collect();
        message.push_str(&format!("\n  try instead: {}", listed.join(" or ")));
    }
    message.push_str(
        "\n  if this is deliberate, re-run with --allow-ecosystem-mismatch \
         (ZED_PKG_ALLOW_ECOSYSTEM_MISMATCH=1)",
    );
    Some(message)
}

#[allow(clippy::too_many_arguments)]
pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    let permissions = InstallPermissions {
        allow_build,
        ..InstallPermissions::default()
    };
    install_with_permissions(
        project,
        cfg,
        frozen,
        mode,
        adapter,
        &permissions,
        target,
        allow_ecosystem_mismatch,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn install_with_permissions(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &InstallPermissions,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    install_with_frozen_policy(
        project,
        cfg,
        frozen,
        mode,
        adapter,
        permissions,
        target,
        true,
        allow_ecosystem_mismatch,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn install_frozen_lock_only_with_permissions(
    project: &Path,
    cfg: &Config,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &InstallPermissions,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    install_with_frozen_policy(
        project,
        cfg,
        true,
        mode,
        adapter,
        permissions,
        target,
        false,
        allow_ecosystem_mismatch,
    )
}

#[allow(clippy::too_many_arguments)]
fn install_with_frozen_policy(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &InstallPermissions,
    target: Option<&str>,
    validate_manifest_requirements: bool,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    let store = Store::new(&cfg.home);
    // Serialize against concurrent `zed install` processes (other terminals,
    // parallel CI runners) writing the store, refs.json, and lockfile.
    let _install_lock = store.install_lock()?;
    install_locked(
        project,
        cfg,
        &store,
        frozen,
        mode,
        adapter,
        permissions,
        target,
        validate_manifest_requirements,
        allow_ecosystem_mismatch,
    )
}

fn validate_frozen_manifest_requirements(
    manifest: &Manifest,
    lock: &Lockfile,
    workspace: Option<&WorkspaceInfo>,
    enforce: bool,
) -> Result<()> {
    if !enforce {
        return Ok(());
    }
    let root_key = manifest.full_name();
    for (key, req_str) in &manifest.dependencies {
        let (org, name) = split_key(key)?;
        if key != &root_key && workspace.is_some_and(|ws| ws.members.contains_key(key)) {
            continue;
        }
        let entry = lock
            .find(&org, &name)
            .with_context(|| format!("--frozen: `{key}` is not in {LOCKFILE_FILE}"))?;
        let req = Requirement::parse(req_str);
        if !req.matches(&entry.version) {
            bail!(
                "--frozen: lockfile pins {key}@{} which no longer satisfies `{req_str}`",
                entry.version
            );
        }
    }
    Ok(())
}

/// Write a newly resolved lockfile. A frozen install is a verifier and
/// materializer only: it must preserve the caller's exact lock bytes,
/// including comments and provenance fields newer than this CLI knows.
fn write_resolved_lockfile(
    lock_path: &Path,
    frozen: bool,
    resolved: &BTreeMap<String, VersionMetadata>,
    registry: &str,
) -> Result<()> {
    if frozen {
        return Ok(());
    }
    let mut lock = Lockfile::default();
    for vm in resolved.values() {
        lock.upsert(LockedPackage {
            org: vm.org.clone(),
            name: vm.name.clone(),
            version: vm.version.clone(),
            sha256: vm.sha256.clone(),
            size: vm.size,
            format: vm.format,
            vcs_tag: vm.vcs_tag.clone(),
            vcs_commit: vm.vcs_commit.clone(),
            source: registry.to_string(),
        });
    }
    fs::write(lock_path, lock.to_toml_string()?)?;
    Ok(())
}

#[cfg(test)]
#[test]
fn frozen_lock_write_preserves_exact_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join(LOCKFILE_FILE);
    let original = b"# retained provenance and formatting
version = 1
";
    fs::write(&lock_path, original).unwrap();

    write_resolved_lockfile(
        &lock_path,
        true,
        &BTreeMap::new(),
        "file:///temporary-registry-mirror",
    )
    .unwrap();

    assert_eq!(fs::read(&lock_path).unwrap(), original);
}

fn locked_version_metadata(locked: &LockedPackage) -> VersionMetadata {
    VersionMetadata {
        org: locked.org.clone(),
        name: locked.name.clone(),
        version: locked.version.clone(),
        sha256: locked.sha256.clone(),
        size: locked.size,
        format: locked.format,
        vcs_tag: locked.vcs_tag.clone(),
        vcs_commit: locked.vcs_commit.clone(),
        // Never consumed while the store or verified cache owns the
        // bytes. If local verification fails, ensure_artifact falls
        // back to the configured registry and reports that failure.
        download_url: String::new(),
        published_at: "1970-01-01T00:00:00Z".to_string(),
        yanked: false,
        mirrors: Vec::new(),
    }
}

#[cfg(test)]
#[test]
fn frozen_local_metadata_preserves_lock_identity_and_provenance() {
    let locked = LockedPackage {
        org: "acme".to_string(),
        name: "tool".to_string(),
        version: "1.2.3".to_string(),
        sha256: "a".repeat(64),
        size: 42,
        format: zed_interfaces::artifact::ArtifactFormat::TarGz,
        vcs_tag: "v1.2.3".to_string(),
        vcs_commit: Some("b".repeat(40)),
        source: "https://registry.invalid".to_string(),
    };
    let metadata = locked_version_metadata(&locked);
    assert_eq!(metadata.org, locked.org);
    assert_eq!(metadata.name, locked.name);
    assert_eq!(metadata.version, locked.version);
    assert_eq!(metadata.sha256, locked.sha256);
    assert_eq!(metadata.size, locked.size);
    assert_eq!(metadata.format, locked.format);
    assert_eq!(metadata.vcs_tag, locked.vcs_tag);
    assert_eq!(metadata.vcs_commit, locked.vcs_commit);
    assert!(metadata.download_url.is_empty());
    assert!(!metadata.yanked);
}

/// Install body, called with the store lock already held. Split out so the
/// build-hook path can install `[build-dependencies]` into a staging dir
/// under the same lock without deadlocking on a re-acquire.
#[allow(clippy::too_many_arguments)]
fn install_locked(
    project: &Path,
    cfg: &Config,
    store: &Store,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &InstallPermissions,
    target: Option<&str>,
    validate_manifest_requirements: bool,
    allow_ecosystem_mismatch: bool,
) -> Result<InstallOutcome> {
    let mode = effective_install_mode(mode);
    let manifest = read_manifest(project)?;
    let configured_adapter = manifest
        .install
        .adapter
        .as_deref()
        .map(named_adapter)
        .transpose()?;
    // CLI selection wins, then the consumer manifest, then project markers.
    // If all three leave us at `none`, each dependency may still request its
    // own adapter. Per-target publish manifests use that last path so a
    // `*-nodejs` package wires itself into node_modules even in a freshly
    // initialized project with no package.json yet.
    let use_dependency_adapters = adapter == Adapter::Auto
        && configured_adapter.is_none()
        && detect_adapter(project) == Adapter::None;
    let adapter = match adapter {
        Adapter::Auto => configured_adapter.unwrap_or_else(|| detect_adapter(project)),
        other => other,
    };
    let resolved_target = resolve_target(project, &manifest, target);
    // Computed once: the guard consults it per dependency.
    let project_ecos = project_ecosystems(project);
    let reg = registry_for(&cfg.registry)?;
    let lock_path = project.join(LOCKFILE_FILE);

    let workspace = find_workspace(project);
    let mut workspace_links: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut resolved: BTreeMap<String, VersionMetadata> = BTreeMap::new();

    if frozen {
        let text = fs::read_to_string(&lock_path)
            .with_context(|| format!("--frozen requires {LOCKFILE_FILE}"))?;
        let lock = Lockfile::parse(&text)?;
        validate_frozen_manifest_requirements(
            &manifest,
            &lock,
            workspace.as_ref(),
            validate_manifest_requirements,
        )?;
        workspace_links =
            collect_workspace_links_for_frozen(project, &manifest, workspace.as_ref())?;
        for locked in &lock.packages {
            if !is_slug(&locked.org) || !is_slug(&locked.name) {
                bail!(
                    "lockfile entry `{}/{}` has an invalid identity; refusing",
                    locked.org,
                    locked.name
                );
            }
            require_sha256(&locked.sha256)?;
            let vm = if store.has(&locked.sha256) || store.cached_artifact(&locked.sha256).is_file()
            {
                // The lock already carries every immutable field needed
                // to authenticate locally cached bytes. Avoid turning an
                // exact frozen replay into a registry availability check.
                locked_version_metadata(locked)
            } else {
                let vm = reg.get_version(&locked.org, &locked.name, &locked.version)?;
                if vm.sha256 != locked.sha256 {
                    bail!(
                        "registry artifact for {}@{} changed (lock {} vs registry {}); refusing",
                        locked.full_name(),
                        locked.version,
                        locked.sha256,
                        vm.sha256
                    );
                }
                vm
            };
            validate_version_metadata(&vm)?;
            resolved.insert(locked.full_name(), vm);
        }
    } else {
        let mut queue: VecDeque<(String, String, String)> = VecDeque::new();
        for (key, req) in &manifest.dependencies {
            let (org, name) = split_key(key)?;
            queue.push_back((org, name, req.clone()));
        }
        while let Some((org, name, req_str)) = queue.pop_front() {
            let key = format!("{org}/{name}");
            // Workspace members short-circuit the registry entirely: link
            // the member's source tree, then keep resolving its deps.
            if let Some(member_dir) =
                workspace_member_for_dependency(&manifest, workspace.as_ref(), &key)
            {
                let member_manifest = read_manifest(member_dir).with_context(|| {
                    format!(
                        "reading workspace member `{key}` from {}",
                        member_dir.display()
                    )
                })?;
                let requirement = Requirement::parse(&req_str);
                if !requirement.matches(&member_manifest.package.version) {
                    bail!(
                        "workspace member {key}@{} does not satisfy `{req_str}`",
                        member_manifest.package.version
                    );
                }
                if member_dir != project && !workspace_links.contains_key(&key) {
                    workspace_links.insert(key.clone(), member_dir.clone());
                    for (sub_key, sub_req) in member_manifest.dependencies {
                        let (sub_org, sub_name) = split_key(&sub_key)?;
                        queue.push_back((sub_org, sub_name, sub_req));
                    }
                }
                continue;
            }
            let req = Requirement::parse(&req_str);
            if let Some(existing) = resolved.get(&key) {
                if req.matches(&existing.version) {
                    continue;
                }
                bail!(
                    "version conflict for {key}: resolved {} but another dependency \
                     requires `{req_str}` (zed installs one version per package)",
                    existing.version
                );
            }
            let pkg = reg.get_package(&org, &name)?;
            // Fresh resolution never selects a yanked version, cargo-style:
            // it falls through to the next-best match, and if the only
            // versions that would satisfy the requirement are yanked it
            // fails with a message that says so. Installs pinned by an
            // existing lockfile keep working because --frozen skips
            // resolution entirely.
            let mut candidates = pkg.versions.clone();
            let mut skipped_yanked: Vec<String> = Vec::new();
            let vm = loop {
                let resolved = version::resolve(&req, &candidates).map(str::to_string);
                let Some(version) = resolved else {
                    if !skipped_yanked.is_empty() {
                        bail!(
                            "the only version(s) of {key} satisfying `{req_str}` are yanked \
                             ({}); existing lockfiles keep working via `zed install --frozen`",
                            skipped_yanked.join(", ")
                        );
                    }
                    bail!(
                        "no version of {key} satisfies `{req_str}` (available: {})",
                        pkg.versions.join(", ")
                    );
                };
                let vm = reg.get_version(&org, &name, &version)?;
                if vm.yanked {
                    candidates.retain(|v| *v != version);
                    skipped_yanked.push(version);
                    continue;
                }
                break vm;
            };
            let pkg_dir = ensure_artifact(reg.as_ref(), store, &vm)?;
            resolved.insert(key.clone(), vm);
            let sub_manifest = read_artifact_manifest(&pkg_dir, &key)?;
            for (sub_key, sub_req) in sub_manifest.dependencies {
                let (sub_org, sub_name) = split_key(&sub_key)?;
                queue.push_back((sub_org, sub_name, sub_req));
            }
        }
    }

    // Resolve and validate every package's install metadata before touching the
    // consumer project. Native host packages are installed as one graph-wide
    // operation so manager conflicts and missing consent fail before a project
    // transaction exists.
    let mut package_sources: BTreeMap<String, PackageSource> = BTreeMap::new();
    let mut native_requirements = Vec::new();
    let mut lifecycle_requirements: Vec<(String, Option<BuildSection>, InstallHooksSection)> =
        Vec::new();
    let root_target = projected_root_target(&manifest, resolved_target.as_deref());
    native_requirements.push(NativeRequirement::new(
        manifest.full_name(),
        manifest.effective_native_dependencies(root_target)?,
    ));

    for (key, vm) in &resolved {
        let dir = ensure_artifact(reg.as_ref(), store, vm)?;
        let package_manifest = Some(read_artifact_manifest(&dir, key)?);
        let (native_dependencies, hooks) =
            package_install_metadata(package_manifest.as_ref(), resolved_target.as_deref())?;
        let dep_build = package_manifest
            .as_ref()
            .and_then(|item| item.build.as_ref());
        let build = manifest.effective_build(key, dep_build);
        lifecycle_requirements.push((key.clone(), build, hooks));
        native_requirements.push(NativeRequirement::new(key.clone(), native_dependencies));
        package_sources.insert(
            key.clone(),
            PackageSource {
                dir,
                manifest: package_manifest,
            },
        );
    }

    let mut workspace_manifests: BTreeMap<String, Manifest> = BTreeMap::new();
    for (key, member_dir) in &workspace_links {
        let member = read_manifest(member_dir).with_context(|| {
            format!(
                "reading workspace package `{key}` from {}",
                member_dir.display()
            )
        })?;
        let (native_dependencies, hooks) =
            package_install_metadata(Some(&member), resolved_target.as_deref())?;
        let build = manifest.effective_build(key, member.build.as_ref());
        lifecycle_requirements.push((key.clone(), build, hooks));
        native_requirements.push(NativeRequirement::new(key.clone(), native_dependencies));
        workspace_manifests.insert(key.clone(), member);
    }

    // Consent and manager compatibility are checked in a stable order before
    // any host package manager runs: native prerequisites first, then all
    // package-authored hooks and builds.
    native::preflight(
        &native_requirements,
        permissions.allow_native_deps,
        permissions.native_manager.as_deref(),
    )?;
    for (key, build, hooks) in &lifecycle_requirements {
        ensure_lifecycle_permissions(key, build.as_ref(), hooks, permissions)?;
    }

    let native_outcome = native::install(
        &native_requirements,
        permissions.allow_native_deps,
        permissions.native_manager.as_deref(),
        cfg.interactive,
        &cfg.home,
    )?;

    let modules_dir = manifest.modules_dir();
    let modules = project.join(modules_dir);
    let previous_lock = fs::read_to_string(&lock_path)
        .ok()
        .and_then(|text| Lockfile::parse(&text).ok())
        .unwrap_or_default();
    let had_node_adapter = project.join(".zed").join("node_path").is_file();
    interactive::confirm(
        cfg.interactive,
        &format!(
            "materialize {} resolved package(s) in {}",
            resolved.len() + workspace_links.len(),
            modules.display()
        ),
    )?;
    let mut transaction = ProjectTransaction::begin(project)?;
    eprintln!(
        "transaction {}: staging install rollback data",
        transaction.id()
    );
    transaction.backup(&modules)?;
    transaction.backup(&lock_path)?;
    transaction.backup(&project.join(".zed").join("node_path"))?;
    transaction.backup(&project.join(".zed").join("classpath"))?;
    if had_node_adapter {
        for locked in &previous_lock.packages {
            transaction.backup(
                &project
                    .join("node_modules")
                    .join(format!("@{}", locked.org))
                    .join(&locked.name),
            )?;
        }
    }

    let mut installed = Vec::new();
    let mut shas = Vec::new();
    let mut jars: Vec<String> = Vec::new();
    let mut bins: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut used_node_adapter = false;
    let mut used_java_adapter = false;
    // Adapters whose wiring is one project-level file rather than per-package
    // links: the installed roots each one needs to list.
    let mut wired_roots: BTreeMap<Adapter, Vec<PathBuf>> = BTreeMap::new();
    // Every installed package, for the adapter-independent `.zed/paths.json`.
    let mut wired_packages: Vec<WiredPackage> = Vec::new();
    for vm in resolved.values() {
        interactive::confirm(
            cfg.interactive,
            &format!("install {}/{}@{}", vm.org, vm.name, vm.version),
        )?;
        let key = format!("{}/{}", vm.org, vm.name);
        let source = package_sources
            .get(&key)
            .with_context(|| format!("resolved package `{key}` has no prepared source"))?;
        let pkg_dir = &source.dir;
        let pkg_manifest = source.manifest.as_ref();
        let (native_dependencies, hooks) =
            package_install_metadata(pkg_manifest, resolved_target.as_deref())?;
        // Package lifecycle preparation swaps the link source from the
        // pristine store entry to a platform cache entry. Hooks and builds run
        // only in a writable staging copy.
        let dep_build = pkg_manifest.and_then(|m| m.build.as_ref());
        let build = manifest.effective_build(&key, dep_build);
        let link_src = prepare_artifact(
            cfg,
            store,
            vm,
            pkg_dir,
            pkg_manifest,
            build.as_ref(),
            &hooks,
            &native_dependencies,
            &native_outcome,
            permissions,
            resolved_target.as_deref(),
            false,
        )?;
        // A per-language package (e.g. `acme-clients-java`) states the
        // ecosystem it is for. Refuse to drop it into a project that has no
        // such toolchain: the files would sit in zed_modules/ unread, and the
        // consumer would debug a "missing" client that installed "fine".
        if !allow_ecosystem_mismatch
            && let Some(pm) = pkg_manifest
            && let Some(problem) = ecosystem_mismatch(
                key.as_str(),
                &vm.name,
                pm.package.language,
                pm.package.ecosystem(),
                &project_ecos,
            )
        {
            bail!("{problem}");
        }
        // Polyglot dependency: narrow the link source to this consumer's
        // language subtree, so a Python project gets `python/` at its import
        // root instead of a tree with the Node and Go sources beside it.
        // Single-language packages are unaffected (target_subdir -> None).
        let link_src = match pkg_manifest {
            Some(pm) => match pm.target_subdir(resolved_target.as_deref()) {
                Ok(Some(subdir)) => {
                    let scoped = link_src.join(subdir);
                    if !scoped.is_dir() {
                        bail!(
                            "package `{key}` declares target `{}` at `{subdir}`, but that \
                             directory is missing from the published artifact",
                            resolved_target.as_deref().unwrap_or_default()
                        );
                    }
                    scoped
                }
                Ok(None) => link_src,
                // An explicit request the package cannot satisfy: fail with
                // the list of targets it does publish rather than installing
                // a tree the consumer's toolchain cannot use.
                Err(err) => bail!("{err}"),
            },
            None => link_src,
        };
        let dest = modules.join(&vm.org).join(&vm.name);
        link_or_copy(&link_src, &dest, mode)?;
        if let Some(pm) = &pkg_manifest {
            for (bin_name, rel_target) in &pm.bin {
                bins.insert(bin_name.clone(), dest.join(rel_target));
            }
        }
        let dependency_adapter = if use_dependency_adapters {
            pkg_manifest
                .as_ref()
                .and_then(|pm| pm.install.adapter.as_deref())
                .map(named_adapter)
                .transpose()?
                .unwrap_or(adapter)
        } else {
            adapter
        };
        match dependency_adapter {
            Adapter::Node => {
                used_node_adapter = true;
                let node_dest = project
                    .join("node_modules")
                    .join(format!("@{}", vm.org))
                    .join(&vm.name);
                transaction.backup(&node_dest)?;
                link_or_copy(&link_src, &node_dest, mode)?;
            }
            Adapter::Java => {
                used_java_adapter = true;
                // Classpath entries point at the project-local link so the
                // file works in both symlink and copy (container) modes.
                for entry in walkdir::WalkDir::new(&dest)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if entry.path().extension().is_some_and(|e| e == "jar") {
                        jars.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
            // Go, Python, Rust and Dart need no per-package linking: their
            // wiring is one project-level file listing the installed roots,
            // written after the loop. Record the root and move on.
            Adapter::Go => wired_roots
                .entry(Adapter::Go)
                .or_default()
                .push(dest.clone()),
            Adapter::Python => wired_roots
                .entry(Adapter::Python)
                .or_default()
                .push(dest.clone()),
            Adapter::Rust => wired_roots
                .entry(Adapter::Rust)
                .or_default()
                .push(dest.clone()),
            Adapter::Dart => wired_roots
                .entry(Adapter::Dart)
                .or_default()
                .push(dest.clone()),
            Adapter::Auto | Adapter::None => {}
        }
        wired_packages.push(WiredPackage {
            key: format!("{}/{}", vm.org, vm.name),
            version: vm.version.clone(),
            language: pkg_manifest
                .map(|pm| pm.package.language)
                .unwrap_or_default(),
            ecosystem: pkg_manifest
                .map(|pm| pm.package.ecosystem())
                .unwrap_or_default(),
            path: dest.clone(),
        });
        installed.push((format!("{}/{}", vm.org, vm.name), vm.version.clone()));
        shas.push(vm.sha256.clone());
    }
    for (key, member_dir) in &workspace_links {
        interactive::confirm(cfg.interactive, &format!("link workspace package {key}"))?;
        let member_manifest = workspace_manifests
            .get(key)
            .with_context(|| format!("workspace package `{key}` has no parsed manifest"))?;
        let (native_dependencies, hooks) =
            package_install_metadata(Some(member_manifest), resolved_target.as_deref())?;
        let build = manifest.effective_build(key, member_manifest.build.as_ref());
        let temporary = prepare_workspace_artifact(
            cfg,
            store,
            key,
            &member_manifest.package.version,
            member_dir,
            member_manifest,
            build.as_ref(),
            &hooks,
            &native_dependencies,
            &native_outcome,
            permissions,
            resolved_target.as_deref(),
        )?;
        let (prepared_source, workspace_mode) = match temporary.as_ref() {
            Some(prepared) => (&prepared.path, InstallMode::Copy),
            None => (member_dir, mode),
        };
        let link_source = match member_manifest.target_subdir(resolved_target.as_deref()) {
            Ok(Some(subdir)) => {
                let scoped = prepared_source.join(subdir);
                if !scoped.is_dir() {
                    bail!(
                        "workspace package `{key}` declares target `{}` at `{subdir}`, but that directory is missing after lifecycle preparation",
                        resolved_target.as_deref().unwrap_or_default()
                    );
                }
                scoped
            }
            Ok(None) => prepared_source.to_path_buf(),
            Err(error) => bail!("{error}"),
        };
        let (org, name) = split_key(key)?;
        let member_manifest = read_manifest(member_dir).with_context(|| {
            format!(
                "reading workspace member `{key}` from {}",
                member_dir.display()
            )
        })?;
        if !allow_ecosystem_mismatch
            && let Some(problem) = ecosystem_mismatch(
                key,
                &name,
                member_manifest.package.language,
                member_manifest.package.ecosystem(),
                &project_ecos,
            )
        {
            bail!("{problem}");
        }

        let dest = modules.join(&org).join(&name);
        // A workspace package with lifecycle commands is copied from its
        // temporary finalized staging tree; a plain workspace package keeps
        // the historical live-link behavior in symlink mode. Adapter wiring
        // must point at the same finalized source rather than bypassing hooks.
        link_or_copy(&link_source, &dest, workspace_mode)?;
        for (bin_name, rel_target) in &member_manifest.bin {
            bins.insert(bin_name.clone(), dest.join(rel_target));
        }

        let dependency_adapter = if use_dependency_adapters {
            member_manifest
                .install
                .adapter
                .as_deref()
                .map(named_adapter)
                .transpose()?
                .unwrap_or(adapter)
        } else {
            adapter
        };
        match dependency_adapter {
            Adapter::Node => {
                used_node_adapter = true;
                let node_dest = project
                    .join("node_modules")
                    .join(format!("@{org}"))
                    .join(&name);
                transaction.backup(&node_dest)?;
                link_or_copy(&link_source, &node_dest, workspace_mode)?;
            }
            Adapter::Java => {
                used_java_adapter = true;
                for entry in walkdir::WalkDir::new(&dest)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                {
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "jar")
                    {
                        jars.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
            Adapter::Go => wired_roots
                .entry(Adapter::Go)
                .or_default()
                .push(dest.clone()),
            Adapter::Python => wired_roots
                .entry(Adapter::Python)
                .or_default()
                .push(dest.clone()),
            Adapter::Rust => wired_roots
                .entry(Adapter::Rust)
                .or_default()
                .push(dest.clone()),
            Adapter::Dart => wired_roots
                .entry(Adapter::Dart)
                .or_default()
                .push(dest.clone()),
            Adapter::Auto | Adapter::None => {}
        }
        wired_packages.push(WiredPackage {
            key: key.clone(),
            version: member_manifest.package.version.clone(),
            language: member_manifest.package.language,
            ecosystem: member_manifest.package.ecosystem(),
            path: dest,
        });
        installed.push((key.clone(), member_manifest.package.version.clone()));
    }
    hoist_bins(&modules, &bins, mode)?;
    if used_java_adapter {
        jars.sort();
        let classpath_file = project.join(".zed").join("classpath");
        fs::create_dir_all(classpath_file.parent().context("classpath parent")?)?;
        fs::write(&classpath_file, jars.join(":") + "\n")?;
        println!(
            "wrote {} ({} jars); use: java -cp \"$(cat .zed/classpath)\" ...",
            classpath_file.display(),
            jars.len()
        );
    }
    if used_node_adapter {
        // Complement npm rather than replace it: the per-package
        // node_modules/@<org>/<name> links above already resolve, and this
        // NODE_PATH points Node at the zed tree root so `require("<org>/<name>")`
        // works too — set `NODE_PATH="$(cat .zed/node_path)"`.
        let node_path_file = project.join(".zed").join("node_path");
        fs::create_dir_all(node_path_file.parent().context("node_path parent")?)?;
        fs::write(&node_path_file, format!("{modules_dir}\n"))?;
        println!(
            "wrote {} ({modules_dir}); use: NODE_PATH=\"$(cat .zed/node_path)\" node ...",
            node_path_file.display()
        );
    }
    write_toolchain_wiring(project, &wired_roots)?;
    write_paths_index(project, modules_dir, &wired_packages)?;

    let transaction_summary = if frozen {
        format!(
            "preserve {LOCKFILE_FILE} byte-for-byte, update project references, and commit transaction {}",
            transaction.id()
        )
    } else {
        format!(
            "write {LOCKFILE_FILE}, update project references, and commit transaction {}",
            transaction.id()
        )
    };
    interactive::confirm(cfg.interactive, &transaction_summary)?;
    write_resolved_lockfile(&lock_path, frozen, &resolved, &cfg.registry)?;
    store.record_project(project, shas)?;
    transaction.commit()?;

    for (name, version) in &installed {
        println!("installed {name}@{version}");
    }
    if !bins.is_empty() {
        let mut names: Vec<&String> = bins.keys().collect();
        names.sort();
        println!(
            "{} bin(s) in {modules_dir}/{BIN_DIR}/ ({}); run with `zed run <name>`",
            names.len(),
            names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!(
        "{} package(s) in {modules_dir}/ ({})",
        installed.len(),
        match mode {
            InstallMode::Symlink => "symlinked from the global store",
            InstallMode::Copy => "copied for container-safe layers",
        }
    );
    Ok(InstallOutcome { installed })
}

/// Remove materialized dependencies without changing the manifest or lock.
///
/// Keeping `.zpkg.lock` is intentional: `zed install --frozen` is the exact
/// inverse and proves uninstall/reinstall reproducibility. Every project-tree
/// mutation is covered by a UUID-v4 transaction.
pub fn uninstall(project: &Path, cfg: &Config, specs: &[String]) -> Result<()> {
    let lock_path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&lock_path)
        .with_context(|| format!("zed uninstall requires {LOCKFILE_FILE}"))?;
    let lock = Lockfile::parse(&text).with_context(|| format!("invalid {LOCKFILE_FILE}"))?;

    // Workspace packages deliberately do not appear in the artifact lock: the
    // lock records immutable registry hashes, while workspace members are live
    // source projections. Reconstruct the active workspace graph from the same
    // manifest boundary used by frozen install so a workspace-only project can
    // still uninstall and later restore its exact materialized graph.
    let manifest = read_manifest(project).ok();
    let workspace = find_workspace(project);
    let workspace_links = match manifest.as_ref() {
        Some(manifest) => {
            collect_workspace_links_for_frozen(project, manifest, workspace.as_ref())?
        }
        None => BTreeMap::new(),
    };
    let total_materialized = lock.packages.len() + workspace_links.len();
    if total_materialized == 0 {
        println!("nothing to uninstall");
        return Ok(());
    }

    let mut targets = BTreeSet::new();
    if specs.is_empty() {
        targets.extend(lock.packages.iter().map(LockedPackage::full_name));
        targets.extend(workspace_links.keys().cloned());
    } else {
        for spec in specs {
            if spec.contains('@') {
                bail!("uninstall accepts package identities without versions (expected org/name)");
            }
            let (org, name) = split_key(spec)?;
            let key = format!("{org}/{name}");
            if workspace_links.contains_key(&key) {
                bail!(
                    "selective uninstall of workspace package `{key}` is not supported; run `zed uninstall` without package arguments to remove the complete materialized graph while retaining {LOCKFILE_FILE}"
                );
            }
            if lock.find(&org, &name).is_none() {
                bail!("{key} is neither pinned by {LOCKFILE_FILE} nor an active workspace package");
            }
            targets.insert(key);
        }
    }

    interactive::confirm(
        cfg.interactive,
        &format!(
            "uninstall {} package(s) from {} while retaining {LOCKFILE_FILE}",
            targets.len(),
            project.display()
        ),
    )?;

    let store = Store::new(&cfg.home);
    let _install_lock = store.install_lock()?;
    let modules_dir = manifest
        .as_ref()
        .map(|manifest| manifest.modules_dir().to_string())
        .unwrap_or_else(|| MODULES_DIR.to_string());
    let modules = project.join(&modules_dir);
    let had_node_adapter = project.join(".zed").join("node_path").is_file();
    let had_java_adapter = project.join(".zed").join("classpath").is_file();
    let uninstall_all = targets.len() == total_materialized;

    let mut transaction = ProjectTransaction::begin(project)?;
    eprintln!(
        "transaction {}: staging uninstall rollback data",
        transaction.id()
    );
    if uninstall_all {
        transaction.backup(&modules)?;
        // These files are all generated projections of the materialized graph.
        // Remove them transactionally on a full uninstall so no toolchain sees
        // stale package paths while the retained lock waits for frozen restore.
        for generated in [
            "paths.json",
            "node_path",
            "classpath",
            "go.work",
            "pythonpath",
            "cargo-paths.toml",
            "pub-deps.yaml",
        ] {
            transaction.backup(&project.join(".zed").join(generated))?;
        }
    } else {
        transaction.backup(&modules.join(BIN_DIR))?;
        if had_java_adapter {
            transaction.backup(&project.join(".zed").join("classpath"))?;
        }
    }

    for key in &targets {
        interactive::confirm(cfg.interactive, &format!("unmaterialize {key}"))?;
        let (org, name) = split_key(key)?;
        if !uninstall_all {
            transaction.backup(&modules.join(&org).join(&name))?;
        }
        if had_node_adapter {
            transaction.backup(
                &project
                    .join("node_modules")
                    .join(format!("@{org}"))
                    .join(&name),
            )?;
        }
    }

    let remaining: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|package| !targets.contains(&package.full_name()))
        .collect();
    let remaining_workspace = workspace_links
        .keys()
        .filter(|key| !targets.contains(*key))
        .count();
    if !uninstall_all {
        let mut bins: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut mode = InstallMode::Copy;
        let mut jars = Vec::new();
        for package in &remaining {
            let installed = modules.join(&package.org).join(&package.name);
            if fs::symlink_metadata(&installed)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                mode = InstallMode::Symlink;
            }
            if let Ok(package_manifest) = read_manifest(&installed) {
                for (name, target) in package_manifest.bin {
                    bins.insert(name, installed.join(target));
                }
            }
            if had_java_adapter && installed.exists() {
                for entry in walkdir::WalkDir::new(&installed)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                {
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "jar")
                    {
                        jars.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
        }
        // Selective uninstall currently targets registry packages only. Keep
        // workspace bins and Java entries in the rebuilt aggregate projections.
        for key in workspace_links.keys().filter(|key| !targets.contains(*key)) {
            let (org, name) = split_key(key)?;
            let installed = modules.join(&org).join(&name);
            if fs::symlink_metadata(&installed)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                mode = InstallMode::Symlink;
            }
            if let Ok(package_manifest) = read_manifest(&installed) {
                for (bin_name, target) in package_manifest.bin {
                    bins.insert(bin_name, installed.join(target));
                }
            }
            if had_java_adapter && installed.exists() {
                for entry in walkdir::WalkDir::new(&installed)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                {
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "jar")
                    {
                        jars.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
        }
        hoist_bins(&modules, &bins, mode)?;
        if had_java_adapter && !jars.is_empty() {
            jars.sort();
            jars.dedup();
            let classpath = project.join(".zed").join("classpath");
            fs::create_dir_all(classpath.parent().context("classpath parent")?)?;
            fs::write(classpath, jars.join(":") + "\n")?;
        }
    }

    let remaining_total = remaining.len() + remaining_workspace;
    interactive::confirm(
        cfg.interactive,
        &format!(
            "record {remaining_total} remaining installed package(s) and commit transaction {}",
            transaction.id()
        ),
    )?;
    store.record_project(
        project,
        remaining
            .iter()
            .map(|package| package.sha256.clone())
            .collect(),
    )?;
    transaction.commit()?;

    for key in &targets {
        println!("uninstalled {key}");
    }
    println!(
        "{remaining_total} package(s) remain materialized; {LOCKFILE_FILE} retained for frozen reinstall"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// package lifecycle preparation (native deps, install hooks, and builds)

/// Cache identity for every package-authored operation that can change the
/// materialized artifact. The immutable source hash is the primary identity;
/// consumer build overrides, target selection, hooks, and the selected native
/// route are hashed into a short suffix.
fn lifecycle_cache_key(
    source_sha: &str,
    build: Option<&BuildSection>,
    hooks: &InstallHooksSection,
    native_dependencies: &NativeDependencies,
    native_outcome: &NativeInstallOutcome,
    target: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"zed-install-lifecycle-v1");
    hasher.update([0]);
    if let Some(build) = build {
        hasher.update(b"build");
        hasher.update([0]);
        hasher.update(build.command.as_bytes());
        hasher.update([0]);
        for output in &build.outputs {
            hasher.update(output.as_bytes());
            hasher.update([0]);
        }
    }
    for (phase, commands) in [
        (b"pre-install".as_slice(), &hooks.pre_install),
        (b"post-install".as_slice(), &hooks.post_install),
    ] {
        hasher.update(phase);
        hasher.update([0]);
        for command in commands {
            hasher.update(command.as_bytes());
            hasher.update([0]);
        }
    }
    if let Some(manager) = &native_outcome.manager {
        hasher.update(b"native-manager");
        hasher.update([0]);
        hasher.update(manager.as_bytes());
        hasher.update([0]);
        for package in native_outcome.packages_for(native_dependencies) {
            hasher.update(package.as_bytes());
            hasher.update([0]);
        }
    }
    if let Some(target) = target {
        hasher.update(b"target");
        hasher.update([0]);
        hasher.update(target.as_bytes());
        hasher.update([0]);
    }
    let lifecycle_hash = hex::encode(hasher.finalize());
    format!("{source_sha}-{}", &lifecycle_hash[..16])
}

fn ensure_lifecycle_permissions(
    key: &str,
    build: Option<&BuildSection>,
    hooks: &InstallHooksSection,
    permissions: &InstallPermissions,
) -> Result<()> {
    if !hooks.is_empty() && !permissions.allow_install_hooks {
        bail!(
            "{key} declares package install hooks; re-run with --allow-install-hooks or ZED_PKG_ALLOW_INSTALL_HOOKS=1"
        );
    }
    if build.is_some() && !hooks.is_empty() && !permissions.allow_build {
        bail!(
            "{key} declares both install hooks and a [build] step; re-run with --allow-build so zed can execute the complete pre-install -> build -> post-install lifecycle"
        );
    }
    Ok(())
}

fn validate_lifecycle_permissions(
    key: &str,
    build: Option<&BuildSection>,
    hooks: &InstallHooksSection,
    permissions: &InstallPermissions,
) -> Result<bool> {
    if build.is_none() && hooks.is_empty() {
        return Ok(false);
    }
    ensure_lifecycle_permissions(key, build, hooks, permissions)?;
    if build.is_some() && !permissions.allow_build {
        eprintln!(
            "warning: {key} declares a [build] step; linking unbuilt source \
             (re-run with --allow-build or ZED_PKG_ALLOW_BUILD=1 to execute it)"
        );
        return Ok(false);
    }
    Ok(true)
}

fn staging_manifest(build_dependencies: BTreeMap<String, String>) -> Manifest {
    Manifest {
        package: PackageSection {
            org: "zed-build".to_string(),
            name: "staging".to_string(),
            version: "0.0.0".to_string(),
            version_scheme: version::VersionScheme::Semver,
            description: None,
            license: None,
            repository: RepositorySection {
                vcs: Vcs::Git,
                url: "https://localhost/zed-build/staging".to_string(),
            },
            keywords: Vec::new(),
            language: Default::default(),
            ecosystem: Default::default(),
            artifacts: Default::default(),
        },
        dependencies: build_dependencies,
        build_dependencies: BTreeMap::new(),
        native_dependencies: NativeDependencies::new(),
        hooks: InstallHooksSection::default(),
        lifecycle: Default::default(),
        publish: PublishSection::default(),
        scripts: ScriptsSection::default(),
        bin: BTreeMap::new(),
        build: None,
        workspace: None,
        overrides: Default::default(),
        install: Default::default(),
        interop: Default::default(),
        targets: Default::default(),
    }
}

fn install_build_dependencies(
    cfg: &Config,
    store: &Store,
    staging: &Path,
    pkg_manifest: Option<&Manifest>,
    permissions: &InstallPermissions,
    native_outcome: &NativeInstallOutcome,
) -> Result<Option<PathBuf>> {
    let build_dependencies = pkg_manifest
        .map(|manifest| manifest.build_dependencies.clone())
        .unwrap_or_default();
    if build_dependencies.is_empty() {
        return Ok(None);
    }

    let deps_dir = staging.join("build-deps");
    fs::create_dir_all(&deps_dir)?;
    write_manifest(&deps_dir, &staging_manifest(build_dependencies))?;
    let nested_permissions = InstallPermissions {
        allow_build: permissions.allow_build,
        allow_native_deps: permissions.allow_native_deps,
        allow_install_hooks: permissions.allow_install_hooks,
        native_manager: native_outcome
            .manager
            .clone()
            .or_else(|| permissions.native_manager.clone()),
    };
    install_locked(
        &deps_dir,
        cfg,
        store,
        false,
        InstallMode::Symlink,
        Adapter::None,
        &nested_permissions,
        None,
        true,
        true,
    )?;
    Ok(Some(deps_dir.join(MODULES_DIR)))
}

#[allow(clippy::too_many_arguments)]
fn configure_lifecycle_command(
    command: &mut Command,
    work: &Path,
    platform: &str,
    key: &str,
    version: &str,
    phase: &str,
    target: Option<&str>,
    build_modules: Option<&Path>,
    native_dependencies: &NativeDependencies,
    native_outcome: &NativeInstallOutcome,
) -> Result<()> {
    command
        .current_dir(work)
        .env("ZED_INSTALL_PHASE", phase)
        .env("ZED_INSTALL_PACKAGE", key)
        .env("ZED_INSTALL_VERSION", version)
        .env("ZED_INSTALL_PLATFORM", platform)
        .env("ZED_INSTALL_ROOT", work)
        .env("ZED_INSTALL_SOURCE", work)
        .env("ZED_BUILD_PLATFORM", platform)
        .env("ZED_BUILD_SRC", work);
    if let Some(target) = target {
        command
            .env("ZED_INSTALL_TARGET", target)
            .env("ZED_BUILD_TARGET", target);
    }
    let mut lifecycle_paths = Vec::new();
    if let Some(modules) = build_modules {
        lifecycle_paths.push(modules.join(BIN_DIR));
        command
            .env("ZED_BUILD_MODULES", modules)
            .env("ZED_INSTALL_MODULES", modules);
    }
    if let Some(profile) = &native_outcome.profile {
        lifecycle_paths.push(profile.join("bin"));
    }
    if !lifecycle_paths.is_empty() {
        lifecycle_paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env(
            "PATH",
            std::env::join_paths(lifecycle_paths).context("constructing lifecycle PATH")?,
        );
    }
    native::environment(command, native_outcome, native_dependencies)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_install_hooks(
    commands: &[String],
    phase: &str,
    work: &Path,
    platform: &str,
    key: &str,
    version: &str,
    target: Option<&str>,
    build_modules: Option<&Path>,
    native_dependencies: &NativeDependencies,
    native_outcome: &NativeInstallOutcome,
) -> Result<()> {
    for (index, hook) in commands.iter().enumerate() {
        println!(
            "running {phase} hook {}/{} for {key}@{version}",
            index + 1,
            commands.len()
        );
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(hook)
            .env("ZED_INSTALL_HOOK_INDEX", (index + 1).to_string());
        configure_lifecycle_command(
            &mut command,
            work,
            platform,
            key,
            version,
            phase,
            target,
            build_modules,
            native_dependencies,
            native_outcome,
        )?;
        let status = command
            .status()
            .with_context(|| format!("running {phase} hook {} for {key}", index + 1))?;
        if !status.success() {
            bail!("{phase} hook {} for {key} failed with {status}", index + 1);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_staged_lifecycle(
    cfg: &Config,
    store: &Store,
    staging: &Path,
    work: &Path,
    key: &str,
    version: &str,
    pkg_manifest: Option<&Manifest>,
    build: Option<&BuildSection>,
    hooks: &InstallHooksSection,
    native_dependencies: &NativeDependencies,
    native_outcome: &NativeInstallOutcome,
    permissions: &InstallPermissions,
    target: Option<&str>,
) -> Result<()> {
    let platform = current_platform();
    let build_modules = install_build_dependencies(
        cfg,
        store,
        staging,
        pkg_manifest,
        permissions,
        native_outcome,
    )?;

    run_install_hooks(
        &hooks.pre_install,
        "pre-install",
        work,
        &platform,
        key,
        version,
        target,
        build_modules.as_deref(),
        native_dependencies,
        native_outcome,
    )?;

    if let Some(build) = build {
        println!("building {key}@{version} for {platform}...");
        let mut command = Command::new("sh");
        command.arg("-c").arg(&build.command);
        configure_lifecycle_command(
            &mut command,
            work,
            &platform,
            key,
            version,
            "build",
            target,
            build_modules.as_deref(),
            native_dependencies,
            native_outcome,
        )?;
        let status = command
            .status()
            .with_context(|| format!("running [build] command for {key}"))?;
        if !status.success() {
            bail!(
                "[build] command for {key} failed with {status} \
                 (override it via [overrides.build.\"{key}\"] in your manifest)"
            );
        }
    }

    run_install_hooks(
        &hooks.post_install,
        "post-install",
        work,
        &platform,
        key,
        version,
        target,
        build_modules.as_deref(),
        native_dependencies,
        native_outcome,
    )?;
    Ok(())
}

fn copy_finalized_artifact(
    work: &Path,
    destination: &Path,
    build: Option<&BuildSection>,
    key: &str,
) -> Result<()> {
    if build.is_none_or(|section| section.outputs.is_empty()) {
        copy_dir(work, destination)?;
        let _ = fs::remove_dir_all(destination.join(MODULES_DIR));
        let _ = fs::remove_file(destination.join(LOCKFILE_FILE));
        let _ = fs::remove_dir_all(destination.join(crate::transaction::STAGING_DIR));
        return Ok(());
    }

    fs::create_dir_all(destination)?;
    for output in &build.expect("checked above").outputs {
        let from = work.join(output);
        let to = destination.join(output);
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else if from.is_file() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        } else {
            bail!("[build] output `{output}` was not produced by the build of {key}");
        }
    }
    let manifest_source = work.join(MANIFEST_FILE);
    if manifest_source.is_file() {
        fs::copy(&manifest_source, destination.join(MANIFEST_FILE))?;
    }
    Ok(())
}

/// Prepare one immutable registry artifact through its package lifecycle and
/// return either the pristine source or a platform-specific cached result.
#[allow(clippy::too_many_arguments)]
fn prepare_artifact(
    cfg: &Config,
    store: &Store,
    vm: &VersionMetadata,
    pkg_dir: &Path,
    pkg_manifest: Option<&Manifest>,
    build: Option<&BuildSection>,
    hooks: &InstallHooksSection,
    native_dependencies: &NativeDependencies,
    native_outcome: &NativeInstallOutcome,
    permissions: &InstallPermissions,
    target: Option<&str>,
    force: bool,
) -> Result<PathBuf> {
    let key = format!("{}/{}", vm.org, vm.name);
    if !validate_lifecycle_permissions(&key, build, hooks, permissions)? {
        return Ok(pkg_dir.to_path_buf());
    }

    let platform = current_platform();
    let cache_key = lifecycle_cache_key(
        &vm.sha256,
        build,
        hooks,
        native_dependencies,
        native_outcome,
        target,
    );
    let prepared = store.build_pkg_dir(&platform, &cache_key);
    if prepared.is_dir() && !force {
        return Ok(prepared);
    }
    let _lock = store.build_lock(&platform, &cache_key)?;
    if prepared.is_dir() {
        if !force {
            return Ok(prepared);
        }
        let _ = fs::remove_dir_all(store.build_entry(&platform, &cache_key));
    }

    println!("preparing {key}@{} for {platform}...", vm.version);
    let staging = tempfile::tempdir()?;
    let work = staging.path().join("pkg");
    copy_dir(pkg_dir, &work)?;
    execute_staged_lifecycle(
        cfg,
        store,
        staging.path(),
        &work,
        &key,
        &vm.version,
        pkg_manifest,
        build,
        hooks,
        native_dependencies,
        native_outcome,
        permissions,
        target,
    )?;

    // `execute_staged_lifecycle` above has already installed build-only
    // dependencies, run pre-install/build/post-install once, and validated the
    // declared outputs. Promotion must never execute package code a second time.
    let entry = store.build_entry(&platform, &cache_key);
    let entry_parent = entry.parent().context("build entry has a parent")?;
    fs::create_dir_all(entry_parent)?;
    let promote_tmp = tempfile::tempdir_in(entry_parent)?;
    let promoted = promote_tmp.path().join("pkg");
    copy_finalized_artifact(&work, &promoted, build, &key)?;
    fs::create_dir_all(&entry)?;
    let promote_path = promote_tmp.keep().join("pkg");
    match fs::rename(&promote_path, &prepared) {
        Ok(()) => {}
        Err(_) if prepared.is_dir() => {
            let _ = fs::remove_dir_all(promote_path.parent().unwrap_or(&promote_path));
        }
        Err(error) => {
            let _ = fs::remove_dir_all(promote_path.parent().unwrap_or(&promote_path));
            return Err(error.into());
        }
    }
    println!("prepared {key}@{} -> {}", vm.version, prepared.display());
    Ok(prepared)
}

struct TemporaryPreparedSource {
    _staging: tempfile::TempDir,
    path: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn prepare_workspace_artifact(
    cfg: &Config,
    store: &Store,
    key: &str,
    version: &str,
    source: &Path,
    manifest: &Manifest,
    build: Option<&BuildSection>,
    hooks: &InstallHooksSection,
    native_dependencies: &NativeDependencies,
    native_outcome: &NativeInstallOutcome,
    permissions: &InstallPermissions,
    target: Option<&str>,
) -> Result<Option<TemporaryPreparedSource>> {
    if !validate_lifecycle_permissions(key, build, hooks, permissions)? {
        return Ok(None);
    }
    let staging = tempfile::tempdir()?;
    let work = staging.path().join("pkg");
    copy_dir(source, &work)?;
    execute_staged_lifecycle(
        cfg,
        store,
        staging.path(),
        &work,
        key,
        version,
        Some(manifest),
        build,
        hooks,
        native_dependencies,
        native_outcome,
        permissions,
        target,
    )?;
    let finalized = staging.path().join("finalized");
    copy_finalized_artifact(&work, &finalized, build, key)?;
    Ok(Some(TemporaryPreparedSource {
        _staging: staging,
        path: finalized,
    }))
}

/// `zed build [--force]` warms the package lifecycle cache for the locked
/// dependency graph. Build commands are explicitly requested by this command;
/// install hooks and native host packages retain their own independent consent.
#[allow(clippy::too_many_arguments)]
pub fn build_cmd(
    project: &Path,
    cfg: &Config,
    force: bool,
    allow_native_deps: bool,
    allow_install_hooks: bool,
    native_manager: Option<&str>,
) -> Result<()> {
    let manifest = read_manifest(project)?;
    let resolved_target = resolve_target(project, &manifest, None);
    let reg = registry_for(&cfg.registry)?;
    let store = Store::new(&cfg.home);
    let _install_lock = store.install_lock()?;
    let lock_path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&lock_path)
        .with_context(|| format!("zed build needs {LOCKFILE_FILE}; run `zed install` first"))?;
    let lock = Lockfile::parse(&text)?;

    let preflight_permissions = InstallPermissions {
        allow_build: true,
        allow_native_deps,
        allow_install_hooks,
        native_manager: native_manager.map(str::to_owned),
    };
    let mut packages = Vec::new();
    let mut native_requirements = vec![NativeRequirement::new(
        manifest.full_name(),
        manifest.effective_native_dependencies(projected_root_target(
            &manifest,
            resolved_target.as_deref(),
        ))?,
    )];
    for locked in &lock.packages {
        let vm = reg.get_version(&locked.org, &locked.name, &locked.version)?;
        if vm.sha256 != locked.sha256 {
            bail!(
                "registry artifact for {}@{} changed (lock {} vs registry {}); refusing",
                locked.full_name(),
                locked.version,
                locked.sha256,
                vm.sha256
            );
        }
        let source = ensure_artifact(reg.as_ref(), &store, &vm)?;
        let package_manifest = Some(read_artifact_manifest(
            &source,
            &format!("{}/{}", locked.org, locked.name),
        )?);
        let key = format!("{}/{}", locked.org, locked.name);
        let (native_dependencies, hooks) =
            package_install_metadata(package_manifest.as_ref(), resolved_target.as_deref())?;
        let dep_build = package_manifest
            .as_ref()
            .and_then(|item| item.build.as_ref());
        let build = manifest.effective_build(&key, dep_build);
        ensure_lifecycle_permissions(&key, build.as_ref(), &hooks, &preflight_permissions)?;
        native_requirements.push(NativeRequirement::new(key, native_dependencies));
        packages.push((vm, source, package_manifest));
    }

    let native_outcome = native::install(
        &native_requirements,
        allow_native_deps,
        native_manager,
        cfg.interactive,
        &cfg.home,
    )?;
    let mut permissions = preflight_permissions;
    permissions.native_manager = native_outcome
        .manager
        .clone()
        .or(permissions.native_manager);

    let mut prepared_count = 0usize;
    for (vm, source, package_manifest) in packages {
        let key = format!("{}/{}", vm.org, vm.name);
        let dep_build = package_manifest
            .as_ref()
            .and_then(|item| item.build.as_ref());
        let build = manifest.effective_build(&key, dep_build);
        let (native_dependencies, hooks) =
            package_install_metadata(package_manifest.as_ref(), resolved_target.as_deref())?;
        if build.is_none() && hooks.is_empty() {
            continue;
        }
        let output = prepare_artifact(
            cfg,
            &store,
            &vm,
            &source,
            package_manifest.as_ref(),
            build.as_ref(),
            &hooks,
            &native_dependencies,
            &native_outcome,
            &permissions,
            resolved_target.as_deref(),
            force,
        )?;
        println!("prepared {key}@{} -> {}", vm.version, output.display());
        prepared_count += 1;
    }
    if prepared_count == 0 {
        println!("no dependencies declare install hooks or a build step");
    } else {
        println!(
            "prepared {prepared_count} package(s) (lifecycle cache: {})",
            store.builds_root().display()
        );
    }
    Ok(())
}

/// Hoist package-declared executables into `zed_modules/.bin/<name>` so
/// `zed run` and PATH-prepending wrappers find them without polluting the OS
/// PATH.
///
/// Hoisted bins are deliberately project-owned copies in both install modes.
/// In symlink mode the package target may resolve into the immutable global
/// store; chmod'ing that target would mutate the shared store inode for every
/// consumer. Copying the usually-small executable and setting permissions on
/// the destination preserves the store while retaining runnable bins.
fn hoist_bins(modules: &Path, bins: &BTreeMap<String, PathBuf>, _mode: InstallMode) -> Result<()> {
    if bins.is_empty() {
        return Ok(());
    }
    let bin_dir = modules.join(BIN_DIR);
    fs::create_dir_all(&bin_dir)?;
    for (name, target) in bins {
        if !target.exists() {
            eprintln!(
                "warning: bin `{name}` points at missing {}; skipping",
                target.display()
            );
            continue;
        }
        let destination = bin_dir.join(name);
        replace_dest(&destination)?;
        fs::copy(target, &destination)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&destination)?.permissions();
            permissions.set_mode(permissions.mode() | 0o111);
            fs::set_permissions(&destination, permissions)?;
        }
    }
    Ok(())
}

/// Relative path from `from_dir` to `target` without touching the
/// filesystem (both are project-local, so component-wise diffing is safe).
fn pathdiff_relative(from_dir: &Path, target: &Path) -> PathBuf {
    let from: Vec<_> = from_dir.components().collect();
    let to: Vec<_> = target.components().collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut rel = PathBuf::new();
    for _ in common..from.len() {
        rel.push("..");
    }
    for component in &to[common..] {
        rel.push(component);
    }
    rel
}

// ---------------------------------------------------------------------------
// run / yank / gc

/// The project's installed-dependency directory name, honoring
/// `[install].dir` when the project has a readable manifest. Commands that
/// only locate an existing tree (`zed run`, `zed remove`) resolve it this way
/// so they agree with wherever `zed install` actually materialized packages;
/// with no manifest they fall back to the default `zed_modules`.
fn project_modules_dir(project: &Path) -> String {
    read_manifest(project)
        .map(|m| m.modules_dir().to_string())
        .unwrap_or_else(|_| MODULES_DIR.to_string())
}

fn hoisted_bin_candidate(bin_dir: &Path, command: &str) -> Option<PathBuf> {
    let exact = bin_dir.join(command);
    if exact.is_file() {
        return Some(exact);
    }
    #[cfg(windows)]
    for extension in ["exe", "cmd", "bat", "ps1"] {
        let candidate = bin_dir.join(command).with_extension(extension);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(windows)]
fn windows_shebang_command(path: &Path) -> Option<Command> {
    let mut file = fs::File::open(path).ok()?;
    let mut prefix = [0_u8; 512];
    let read = file.read(&mut prefix).ok()?;
    let first_line = std::str::from_utf8(&prefix[..read]).ok()?.lines().next()?;
    let shebang = first_line.strip_prefix("#!")?.trim();
    let mut parts = shebang.split_whitespace();
    let raw_interpreter = parts.next()?;
    let raw_name = Path::new(raw_interpreter).file_name()?.to_string_lossy();
    let interpreter = if raw_name.eq_ignore_ascii_case("env") {
        parts.next()?.to_string()
    } else {
        raw_name.into_owned()
    };
    let mut command = Command::new(interpreter);
    command.args(parts).arg(path);
    Some(command)
}

#[cfg(windows)]
fn command_for_hoisted_bin(path: &Path) -> Command {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("cmd" | "bat") => {
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/S", "/C"]).arg(path);
            command
        }
        Some("ps1") => {
            let mut command = Command::new("powershell.exe");
            command
                .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
                .arg(path);
            command
        }
        _ => windows_shebang_command(path).unwrap_or_else(|| Command::new(path)),
    }
}

/// `zed run <command>` — run a hoisted dependency binary (from
/// `<install.dir>/.bin`, default `zed_modules/.bin`) or any command, with that
/// directory prepended to PATH — npx-style, without polluting the OS PATH
/// (zed-docs issue #7). Returns the child's exit code.
pub fn run(project: &Path, command: &str, args: &[String]) -> Result<i32> {
    let modules_dir = project_modules_dir(project);
    let bin_dir = project.join(&modules_dir).join(BIN_DIR);
    let hoisted = hoisted_bin_candidate(&bin_dir, command);
    let mut paths: Vec<PathBuf> = vec![bin_dir.clone()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let new_path = std::env::join_paths(&paths).context("assembling PATH for zed run")?;
    // Prefer a hoisted bin by absolute path; otherwise fall through to a
    // normal PATH lookup (with .bin still prepended for the child's tools).
    let program = hoisted.as_deref().unwrap_or_else(|| Path::new(command));
    #[cfg(windows)]
    let mut child = if hoisted.is_some() {
        command_for_hoisted_bin(program)
    } else {
        Command::new(program)
    };
    #[cfg(not(windows))]
    let mut child = Command::new(program);
    let status = child
        .args(args)
        .env("PATH", &new_path)
        .current_dir(project)
        .status();
    match status {
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(_) => {
            let available: Vec<String> = fs::read_dir(&bin_dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            bail!(
                "failed to run `{command}` — not a hoisted bin in {modules_dir}/{BIN_DIR}/ \
                 (available: {}) nor on PATH; packages expose binaries via their [bin] table",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            )
        }
    }
}

/// `zed yank org/name@version [--undo]`.
pub fn yank(cfg: &Config, spec: &str, undo: bool) -> Result<()> {
    let (key, version) = spec.split_once('@').context("expected org/name@version")?;
    let (org, name) = split_key(key)?;
    let reg = registry_for(&cfg.registry)?;
    let token = cfg.resolve_token()?;
    let response = reg.yank(&org, &name, version, !undo, token.as_deref())?;
    println!(
        "{} {}/{}@{}",
        if response.yanked {
            "yanked"
        } else {
            "restored"
        },
        response.org,
        response.name,
        response.version
    );
    Ok(())
}

fn parse_age(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    let (num, secs) = match s.chars().last() {
        Some('d') => (&s[..s.len() - 1], 86_400u64),
        Some('h') => (&s[..s.len() - 1], 3_600),
        Some('w') => (&s[..s.len() - 1], 604_800),
        _ => (s, 86_400),
    };
    let n: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid duration `{s}` (use e.g. 90d, 2w, 12h)"))?;
    // Saturate rather than overflow-panic on hostile input like `u64::MAX`d;
    // an absurdly large age simply means "prune nothing".
    Ok(std::time::Duration::from_secs(n.saturating_mul(secs)))
}

/// `zed gc`: least-recently-used garbage collection of the store, build
/// cache, and downloads by last use (zed-docs issue #7). Entries still
/// referenced by a live project are always kept.
pub fn gc(cfg: &Config, older_than: &str, dry_run: bool) -> Result<()> {
    let store = Store::new(&cfg.home);
    let _install_lock = store.install_lock()?;
    let age = parse_age(older_than)?;
    let report = store.gc(age, dry_run)?;
    println!(
        "gc: {} {} across {} store/build entr{} and {} cached download(s) not used in {older_than}",
        if report.dry_run {
            "would reclaim"
        } else {
            "reclaimed"
        },
        human_size(report.freed),
        report.entries_removed,
        if report.entries_removed == 1 {
            "y"
        } else {
            "ies"
        },
        report.cache_files_removed,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// add / remove

/// Candidate package names to try for `name`, in priority order, when routing a
/// multi-language repo to the variant this project can build against.
///
/// A repo publishing per-language packages has no package at its bare name — a
/// polyglot `zed publish` emits only `<name>-<language>`. So `zed add
/// acme/acme-clients` in a Gradle project should reach `acme-clients-java`, and
/// `zed add acme/acme-clients-node` should reach `-nodejs` when that is the
/// spelling the author chose. Both are the same operation: append or swap a
/// language suffix and see what exists.
///
/// Returns names *excluding* `name` itself; the caller tries the exact name
/// first so an existing package always wins over any inference.
fn language_route_candidates(name: &str, project_language: Option<&str>) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut push = |candidate: String| {
        if candidate != name && !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };

    // Case 1: the name already ends in a language this project shares — the
    // author just spelled it differently (`-node` vs `-nodejs`, `-go` vs
    // `-golang`). Offer every synonym of that language.
    for (token, language) in KNOWN_LANGUAGE_TOKENS {
        if let Some(base) = name.strip_suffix(&format!("-{token}")) {
            for (other, other_language) in KNOWN_LANGUAGE_TOKENS {
                if other_language == language {
                    push(format!("{base}-{other}"));
                }
            }
        }
    }

    // Case 2: a bare repo name plus what this project is. Try the canonical
    // token first, then its synonyms, so `-nodejs` beats `-js`.
    if let Some(detected) = project_language
        && let Some(language) = Language::from_token(detected)
        && !language.is_default()
    {
        push(format!("{name}-{}", language.as_str()));
        for (token, token_language) in KNOWN_LANGUAGE_TOKENS {
            if *token_language == language {
                push(format!("{name}-{token}"));
            }
        }
    }
    candidates
}

/// Suffix spellings worth probing, paired with the language they mean. Only the
/// forms a package author plausibly publishes under — not every alias
/// [`Language::from_token`] accepts, since each entry costs a registry lookup.
const KNOWN_LANGUAGE_TOKENS: &[(&str, Language)] = &[
    ("nodejs", Language::Nodejs),
    ("node", Language::Nodejs),
    ("typescript", Language::Nodejs),
    ("ts", Language::Nodejs),
    ("js", Language::Nodejs),
    ("golang", Language::Golang),
    ("go", Language::Golang),
    ("python", Language::Python),
    ("py", Language::Python),
    ("java", Language::Java),
    ("kotlin", Language::Kotlin),
    ("rust", Language::Rust),
    ("dart", Language::Dart),
    ("ruby", Language::Ruby),
    ("php", Language::Php),
    ("csharp", Language::Csharp),
    ("dotnet", Language::Csharp),
    ("swift", Language::Swift),
    ("gleam", Language::Gleam),
];

pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    let (rest, req) = match spec.split_once('@') {
        Some((rest, req)) => (rest.to_string(), Some(req.to_string())),
        None => (spec.to_string(), None),
    };
    let (org, mut name) = split_key(&rest)?;
    let req = match req {
        Some(req) => {
            if req.trim().is_empty() {
                bail!("empty requirement for {org}/{name}");
            }
            // Any non-empty spec is valid: a semver range or an opaque tag.
            req
        }
        None => {
            let reg = registry_for(&cfg.registry)?;
            // Exact name first: an existing package always beats inference.
            let pkg = match reg.get_package(&org, &name) {
                Ok(pkg) => pkg,
                Err(original) => {
                    // Nothing published under that name. Route to the language
                    // variant this project can actually consume, rather than
                    // making the user work out the suffix themselves.
                    let detected = detect_target(project);
                    let candidates = language_route_candidates(&name, detected.as_deref());
                    let routed = candidates
                        .iter()
                        .find_map(|candidate| {
                            reg.get_package(&org, candidate)
                                .ok()
                                .map(|pkg| (candidate.clone(), pkg))
                        })
                        .ok_or_else(|| {
                            if candidates.is_empty() {
                                original
                            } else {
                                anyhow::anyhow!(
                                    "no package `{org}/{name}`; also tried {} for this project",
                                    candidates
                                        .iter()
                                        .map(|c| format!("`{org}/{c}`"))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            }
                        })?;
                    println!(
                        "{org}/{name} is not published; using {org}/{} for this project{}",
                        routed.0,
                        detected
                            .as_deref()
                            .map(|d| format!(" ({d})"))
                            .unwrap_or_default()
                    );
                    name = routed.0;
                    routed.1
                }
            };
            let latest = pkg
                .latest
                .with_context(|| format!("{org}/{name} has no published versions"))?;
            // Caret-range a semver-ish latest; pin an opaque tag exactly.
            match version::parse_version(&latest) {
                Some(_) => format!("^{latest}"),
                None => latest,
            }
        }
    };
    let mut manifest = read_manifest(project)?;
    manifest
        .dependencies
        .insert(format!("{org}/{name}"), req.clone());
    interactive::confirm(
        cfg.interactive,
        &format!("add {org}/{name} = \"{req}\" to {MANIFEST_FILE}"),
    )?;
    let manifest_text = manifest.to_toml_string()?;
    // Install against the proposed manifest in memory. The persistent
    // manifest changes only after the transactional install succeeds.
    crate::config::with_manifest_override(project, manifest_text, || {
        install(
            project,
            cfg,
            false,
            InstallMode::Symlink,
            Adapter::None,
            false,
            // Re-install after the manifest edit; the target comes from
            // [install].target or project inference, same as a bare `zed install`.
            None,
            false,
        )
        .map(|_| ())
    })?;
    let mut manifest_transaction = ProjectTransaction::begin(project)?;
    manifest_transaction.backup(&project.join(MANIFEST_FILE))?;
    write_manifest(project, &manifest)?;
    manifest_transaction.commit()?;
    println!("added {org}/{name} = \"{req}\"");
    Ok(())
}

pub fn remove(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    let (org, name) = split_key(spec)?;
    let mut manifest = read_manifest(project)?;
    if manifest
        .dependencies
        .remove(&format!("{org}/{name}"))
        .is_none()
    {
        bail!("{org}/{name} is not a dependency");
    }
    interactive::confirm(
        cfg.interactive,
        &format!("remove {org}/{name} from {MANIFEST_FILE} and reinstall"),
    )?;
    let manifest_text = manifest.to_toml_string()?;
    crate::config::with_manifest_override(project, manifest_text, || {
        install(
            project,
            cfg,
            false,
            InstallMode::Symlink,
            Adapter::None,
            false,
            // Re-install after the manifest edit; the target comes from
            // [install].target or project inference, same as a bare `zed install`.
            None,
            false,
        )
        .map(|_| ())
    })?;
    let mut manifest_transaction = ProjectTransaction::begin(project)?;
    manifest_transaction.backup(&project.join(MANIFEST_FILE))?;
    write_manifest(project, &manifest)?;
    manifest_transaction.commit()?;
    // Unlink from wherever install put it ([install].dir, default zed_modules),
    // otherwise a relocated tree keeps a stale copy of a removed dependency.
    let dest = project.join(manifest.modules_dir()).join(&org).join(&name);
    replace_dest(&dest)?;
    println!("removed {org}/{name}");
    Ok(())
}

// ---------------------------------------------------------------------------
// pack / publish

pub fn pack_cmd(project: &Path, out: Option<&Path>) -> Result<Vec<pack::PackagedTarget>> {
    let manifest = read_manifest(project)?;
    let packages = pack::pack_all(project, &manifest, out)?;
    for package in &packages {
        println!(
            "packed {}@{}{}",
            package.manifest.full_name(),
            package.manifest.package.version,
            package
                .target
                .as_deref()
                .map(|target| format!(" (target {target})"))
                .unwrap_or_default()
        );
        println!("  {}", package.packed.path.display());
        println!(
            "  sha256 {}\n  size {} ({} files, {} excluded by publish rules)",
            package.packed.sha256,
            human_size(package.packed.size),
            package.packed.file_count,
            package.packed.excluded_count
        );
    }
    Ok(packages)
}

pub fn build_publish_meta(
    manifest: &Manifest,
    packed: &PackResult,
    commit: Option<String>,
) -> PublishMeta {
    PublishMeta {
        manifest: manifest.clone(),
        vcs_tag: manifest.vcs_tag(),
        vcs_commit: commit,
        sha256: packed.sha256.clone(),
        size: packed.size,
        format: packed.format,
    }
}

pub fn publish(
    project: &Path,
    cfg: &Config,
    dry_run: bool,
    allow_dirty: bool,
    skip_vcs_checks: bool,
) -> Result<()> {
    let manifest = read_manifest(project)?;
    let tag = manifest.vcs_tag();
    let vcs = manifest.package.repository.vcs;

    let commit = if skip_vcs_checks {
        eprintln!("warning: --skip-vcs-checks set; publishing without {vcs} tag verification");
        None
    } else {
        Some(verify_publish_provenance(vcs, project, &tag, allow_dirty)?)
    };

    let packages = pack_cmd(project, None)?;

    if dry_run {
        for package in &packages {
            let meta = build_publish_meta(&package.manifest, &package.packed, commit.clone());
            println!(
                "dry run: would publish {}@{} (tag {}, sha256 {}) to {}",
                package.manifest.full_name(),
                package.manifest.package.version,
                meta.vcs_tag,
                meta.sha256,
                cfg.registry
            );
            if let Some(identity) =
                zed_interfaces::parse_github_identity(&package.manifest.package.repository.url)
            {
                println!(
                    "dry run: would ensure git tag {} on {}",
                    meta.vcs_tag,
                    identity.web_url()
                );
                println!(
                    "dry run: would push OCI artifact to {} ({})",
                    zed_interfaces::ghcr_reference(&identity, &meta.vcs_tag),
                    zed_interfaces::github_packages_web_url(&identity)
                );
            }
        }
        return Ok(());
    }

    let reg = registry_for(&cfg.registry)?;
    let token = cfg.resolve_token()?;
    let hermetic_registry = cfg.registry.starts_with("file://")
        || crate::source_fallback::is_loopback_registry(&cfg.registry);
    for package in &packages {
        let meta = build_publish_meta(&package.manifest, &package.packed, commit.clone());
        let identity = &package.manifest.package;
        // Multi-package releases cannot be a single HTTP transaction. Make
        // retries safe instead: an already-published byte-identical target is
        // accepted, while a same-version/different-hash target remains an
        // immutable-version error.
        let mut published_registry = false;
        match reg.get_version(&identity.org, &identity.name, &identity.version) {
            Ok(existing) if existing.sha256 == meta.sha256 => {
                println!(
                    "already published {}/{}@{} with identical sha256; skipping",
                    identity.org, identity.name, identity.version
                );
                published_registry = true;
            }
            Ok(existing) => {
                bail!(
                    "{}/{}@{} already exists with sha256 {}; refusing to replace it with {}",
                    identity.org,
                    identity.name,
                    identity.version,
                    existing.sha256,
                    meta.sha256
                );
            }
            Err(_) => {
                interactive::confirm(
                    cfg.interactive,
                    &format!(
                        "publish {}/{}@{} (sha256 {}) to {}",
                        identity.org, identity.name, identity.version, meta.sha256, cfg.registry
                    ),
                )?;
                match reg.publish(&meta, &package.packed.path, token.as_deref()) {
                    Ok(response) => {
                        println!(
                            "published {}/{}@{} to {}",
                            response.org, response.name, response.version, cfg.registry
                        );
                        published_registry = true;
                    }
                    Err(error) => {
                        eprintln!(
                            "warning: registry publish failed for {}/{}@{} ({error}); trying GitHub Release",
                            identity.org, identity.name, identity.version
                        );
                        if hermetic_registry {
                            return Err(error);
                        }
                    }
                }
            }
        }

        if hermetic_registry {
            continue;
        }

        let download_url = format!(
            "{}{}",
            cfg.registry.trim_end_matches('/'),
            zed_interfaces::registry::artifact_path(&meta.sha256)
        );
        let mut published_github = false;
        match crate::github_mirror::mirror_packed_release(
            &package.manifest,
            &package.packed,
            &meta.vcs_tag,
            meta.vcs_commit.as_deref(),
            &download_url,
        ) {
            Ok(crate::github_mirror::MirrorOutcome::Uploaded {
                owner,
                repo,
                tag,
                asset,
            }) => {
                published_github = true;
                println!(
                    "mirrored {}/{}@{} to github.com/{owner}/{repo} release {tag} ({asset})",
                    identity.org, identity.name, identity.version
                );
            }
            Ok(crate::github_mirror::MirrorOutcome::Skipped(reason)) => {
                if !published_registry {
                    eprintln!(
                        "warning: GitHub release mirror skipped ({reason}) for {}/{}@{}",
                        identity.org, identity.name, identity.version
                    );
                }
            }
            Err(error) => {
                if published_registry {
                    eprintln!(
                        "warning: GitHub release mirror failed for {}/{}@{} ({error})",
                        identity.org, identity.name, identity.version
                    );
                } else {
                    eprintln!(
                        "warning: GitHub release mirror failed for {}/{}@{} ({error}); trying GitHub Packages",
                        identity.org, identity.name, identity.version
                    );
                }
            }
        }

        let hosted = published_registry || published_github;
        match crate::github_packages::mirror_packed_ghcr(
            &package.manifest,
            &package.packed,
            &meta.vcs_tag,
            meta.vcs_commit.as_deref(),
        ) {
            Ok(crate::github_packages::GhcrOutcome::Uploaded {
                reference,
                digest,
                web_url,
            }) => {
                println!(
                    "mirrored {}/{}@{} to GitHub Packages {reference} ({digest})\n  {web_url}",
                    identity.org, identity.name, identity.version
                );
            }
            Ok(crate::github_packages::GhcrOutcome::Skipped(reason)) => {
                if !hosted {
                    bail!(
                        "registry unreachable and GitHub mirrors skipped (release + packages: {reason}) for {}/{}@{}",
                        identity.org,
                        identity.name,
                        identity.version
                    );
                }
            }
            Err(error) => {
                if hosted {
                    eprintln!(
                        "warning: GitHub Packages (GHCR) mirror failed for {}/{}@{} ({error})",
                        identity.org, identity.name, identity.version
                    );
                } else {
                    return Err(error).context(
                        "registry, GitHub Release, and GitHub Packages publish all failed",
                    );
                }
            }
        }
    }
    Ok(())
}

// The r2g roundtrip check (`zed r2g`, alias `zed test-local`) lives in the
// `r2g` module; it composes `pack`, the `file://` registry, and `install`
// from here into a consume-your-own-artifact test.

// ---------------------------------------------------------------------------
// find / login / org / store / cache

pub fn find(cfg: &Config, query: &str) -> Result<()> {
    let reg = registry_for(&cfg.registry)?;
    let results = reg.search(query)?;
    if results.items.is_empty() {
        println!("no packages matched `{query}`");
        return Ok(());
    }
    let width = results
        .items
        .iter()
        .map(|i| i.org.len() + i.name.len() + 1)
        .max()
        .unwrap_or(0);
    for item in results.items {
        let full = format!("{}/{}", item.org, item.name);
        println!(
            "{full:<width$}  {:<10}  {}",
            item.latest.as_deref().unwrap_or("-"),
            item.description.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

pub fn login(cfg: &Config) -> Result<()> {
    let token = match &cfg.token {
        Some(token) => token.clone(),
        None => {
            eprint!("token for {}: ", cfg.registry);
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            line.trim().to_string()
        }
    };
    if token.is_empty() {
        bail!("no token provided (pass --token, set ZED_PKG_TOKEN, or type one)");
    }
    let mut credentials = Credentials::load(&cfg.home)?;
    credentials.set_token(&cfg.registry, token);
    credentials.save(&cfg.home)?;
    println!("saved token for {}", cfg.registry);
    Ok(())
}

pub fn org_claim(cfg: &Config, slug: &str) -> Result<()> {
    if !zed_interfaces::manifest::is_slug(slug) {
        bail!("invalid org slug `{slug}` (lowercase letters, digits, hyphens)");
    }
    let reg = registry_for(&cfg.registry)?;
    let token = cfg.resolve_token()?;
    let response = reg.claim_org(slug, token.as_deref())?;
    if response.created {
        println!("claimed org `{}`", response.slug);
    } else {
        println!("org `{}` already exists", response.slug);
    }
    Ok(())
}

/// `zed org audit <slug>` — print the org's audit trail, newest first
/// (owner-scoped; zed-docs issue #7 governance).
pub fn org_audit(cfg: &Config, slug: &str, limit: Option<u64>) -> Result<()> {
    if !zed_interfaces::manifest::is_slug(slug) {
        bail!("invalid org slug `{slug}` (lowercase letters, digits, hyphens)");
    }
    let reg = registry_for(&cfg.registry)?;
    let token = cfg.resolve_token()?;
    let log = reg.audit_log(slug, limit, token.as_deref())?;
    if log.entries.is_empty() {
        println!("no audit entries for org `{}`", log.org);
        return Ok(());
    }
    // Widths from the data so columns line up without truncating real values.
    let action_w = log
        .entries
        .iter()
        .map(|e| e.action.len())
        .max()
        .unwrap_or(6);
    let actor_w = log
        .entries
        .iter()
        .map(|e| e.actor_token_name.len() + e.actor_role.len() + 2)
        .max()
        .unwrap_or(10);
    for entry in &log.entries {
        let actor = format!("{}({})", entry.actor_token_name, entry.actor_role);
        println!(
            "{}  {:<action_w$}  {:<actor_w$}  {}{}",
            entry.at,
            entry.action,
            actor,
            entry.subject,
            entry
                .detail
                .as_deref()
                .map(|d| format!("  [{d}]"))
                .unwrap_or_default()
        );
    }
    println!(
        "{} entr{}",
        log.entries.len(),
        if log.entries.len() == 1 { "y" } else { "ies" }
    );
    Ok(())
}

pub fn store_status(cfg: &Config) -> Result<()> {
    let store = Store::new(&cfg.home);
    let (count, store_bytes, cache_bytes) = store.status();
    println!(
        "store  {}  ({count} packages, {})",
        store.root().display(),
        human_size(store_bytes)
    );
    println!(
        "cache  {}  ({})",
        store.cache_dir().display(),
        human_size(cache_bytes)
    );
    let build_bytes = store.build_size();
    if build_bytes > 0 {
        println!(
            "builds {}  ({})",
            store.builds_root().display(),
            human_size(build_bytes)
        );
    }
    Ok(())
}

pub fn store_prune(cfg: &Config) -> Result<()> {
    let store = Store::new(&cfg.home);
    let (removed, freed) = store.prune()?;
    println!(
        "pruned {removed} store entries, freed {}",
        human_size(freed)
    );
    Ok(())
}

pub fn cache_clean(cfg: &Config) -> Result<()> {
    let store = Store::new(&cfg.home);
    let freed = store.clean_cache()?;
    println!("cleaned cache, freed {}", human_size(freed));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parse_age_units_and_default() {
        assert_eq!(parse_age("90d").unwrap(), Duration::from_secs(90 * 86_400));
        assert_eq!(parse_age("2w").unwrap(), Duration::from_secs(2 * 604_800));
        assert_eq!(parse_age("12h").unwrap(), Duration::from_secs(12 * 3_600));
        // A bare number means days, and surrounding whitespace is tolerated.
        assert_eq!(parse_age("30").unwrap(), Duration::from_secs(30 * 86_400));
        assert_eq!(
            parse_age("  7d  ").unwrap(),
            Duration::from_secs(7 * 86_400)
        );
        assert_eq!(parse_age("0d").unwrap(), Duration::ZERO);
    }

    #[test]
    fn parse_age_rejects_garbage() {
        for bad in ["abc", "", "d", "w", "-3d", "1.5d", "3 d d"] {
            assert!(parse_age(bad).is_err(), "`{bad}` must not parse");
        }
    }

    #[test]
    fn parse_age_saturates_instead_of_overflowing() {
        // u64::MAX weeks would overflow a naive multiply; the saturating path
        // must yield a (uselessly huge, but valid) duration, never a panic.
        let age = parse_age(&format!("{}w", u64::MAX)).unwrap();
        assert_eq!(age, Duration::from_secs(u64::MAX));
    }

    #[test]
    fn go_workspace_paths_are_relative_to_the_generated_file() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("consumer");
        let package = project.join("zed_modules/acme/tool");
        fs::create_dir_all(&package).unwrap();
        let roots = BTreeMap::from([(Adapter::Go, vec![package])]);

        write_toolchain_wiring(&project, &roots).unwrap();

        let document = fs::read_to_string(project.join(".zed/go.work")).unwrap();
        assert!(document.contains("\t..\n"), "{document}");
        assert!(
            document.contains("\t../zed_modules/acme/tool\n"),
            "{document}"
        );
        assert!(!document.contains("\t./\n"), "{document}");
        assert!(!document.contains("\t./zed_modules"), "{document}");
    }

    #[test]
    fn go_workspace_uses_the_highest_module_go_directive() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("consumer");
        let first = project.join("zed_modules/acme/first");
        let second = project.join("zed_modules/acme/second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(
            project.join("go.mod"),
            r#"module example.com/app

go 1.22
"#,
        )
        .unwrap();
        fs::write(
            first.join("go.mod"),
            r#"module example.com/first

go 1.21
"#,
        )
        .unwrap();
        fs::write(
            second.join("go.mod"),
            r#"module example.com/second

go 1.24.1 // minimum toolchain
"#,
        )
        .unwrap();
        let roots = BTreeMap::from([(Adapter::Go, vec![first, second])]);

        write_toolchain_wiring(&project, &roots).unwrap();

        let document = fs::read_to_string(project.join(".zed/go.work")).unwrap();
        assert_eq!(document.lines().next(), Some("go 1.24.1"), "{document}");
    }

    #[test]
    fn malformed_go_directives_do_not_lower_the_safe_default() {
        assert_eq!(required_go_work_version(Path::new("/missing"), &[]), "1.21");
        assert!(parse_go_directive("module example.com/app go latest").is_none());
        assert!(parse_go_directive("go 1").is_none());
    }

    #[test]
    fn rust_cargo_config_introduces_an_unpublished_crate_by_package_name() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("consumer");
        let package = project.join("zed_modules/acme/tool");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("Cargo.toml"),
            r#"[package]
name = "tool-crate"
version = "1.2.3"
edition = "2021"
"#,
        )
        .unwrap();
        let roots = BTreeMap::from([(Adapter::Rust, vec![package])]);

        write_toolchain_wiring(&project, &roots).unwrap();

        let generated = fs::read_to_string(project.join(".zed/cargo-paths.toml")).unwrap();
        let parsed: toml::Value = toml::from_str(&generated).unwrap();
        assert_eq!(parsed["paths"][0].as_str(), Some("zed_modules/acme/tool"));
        assert_eq!(
            parsed["patch"]["crates-io"]["tool-crate"]["path"].as_str(),
            Some("zed_modules/acme/tool")
        );
        assert!(generated.contains("merge this fragment into .cargo/config.toml"));
    }

    #[test]
    fn rust_cargo_config_rejects_two_paths_for_one_crate_name() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("consumer");
        let first = project.join("zed_modules/acme/first");
        let second = project.join("zed_modules/acme/second");
        for package in [&first, &second] {
            fs::create_dir_all(package).unwrap();
            fs::write(
                package.join("Cargo.toml"),
                r#"[package]
name = "duplicate-crate"
version = "1.0.0"
"#,
            )
            .unwrap();
        }
        let roots = BTreeMap::from([(Adapter::Rust, vec![first, second])]);

        let error = write_toolchain_wiring(&project, &roots)
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate-crate"), "{error}");
        assert!(error.contains("two paths"), "{error}");
    }

    #[test]
    fn frozen_workspace_resolution_expands_transitive_members_and_validates_versions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let app = root.join("apps/cli");
        let utils = root.join("packages/utils");
        let core = root.join("packages/core");
        for directory in [&app, &utils, &core] {
            fs::create_dir_all(directory).unwrap();
        }

        fs::write(
            root.join(MANIFEST_FILE),
            r#"[package]
org = "zedtest"
name = "workspace-root"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/workspace-root"

[workspace]
members = ["packages/*", "apps/*"]
"#,
        )
        .unwrap();
        fs::write(
            core.join(MANIFEST_FILE),
            r#"[package]
org = "zedtest"
name = "ws-core"
version = "1.2.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/ws-core"
"#,
        )
        .unwrap();
        fs::write(
            utils.join(MANIFEST_FILE),
            r#"[package]
org = "zedtest"
name = "ws-utils"
version = "1.1.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/ws-utils"

[dependencies]
"zedtest/ws-core" = "^1"
"#,
        )
        .unwrap();
        fs::write(
            app.join(MANIFEST_FILE),
            r#"[package]
org = "zedtest"
name = "ws-cli"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/ws-cli"

[dependencies]
"zedtest/ws-utils" = "^1"
"#,
        )
        .unwrap();

        let manifest = read_manifest(&app).unwrap();
        let workspace = find_workspace(&app).unwrap();
        let links = collect_workspace_links_for_frozen(&app, &manifest, Some(&workspace)).unwrap();
        assert_eq!(
            links.keys().cloned().collect::<Vec<_>>(),
            vec![
                "zedtest/ws-core".to_string(),
                "zedtest/ws-utils".to_string()
            ]
        );
        assert_eq!(links["zedtest/ws-core"], core);
        assert_eq!(links["zedtest/ws-utils"], utils);

        let incompatible = manifest
            .to_toml_string()
            .unwrap()
            .replace("\"^1\"", "\"^2\"");
        fs::write(app.join(MANIFEST_FILE), incompatible).unwrap();
        let manifest = read_manifest(&app).unwrap();
        let error = collect_workspace_links_for_frozen(&app, &manifest, Some(&workspace))
            .unwrap_err()
            .to_string();
        assert!(error.contains("ws-utils@1.1.0"), "{error}");
        assert!(error.contains("does not satisfy `^2`"), "{error}");
    }

    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
        let manifest = Manifest::parse(
            r#"
[package]
org = "consumer"
name = "app"
version = "0.0.0"

[package.repository]
vcs = "git"
url = "https://localhost/consumer/app"

[dependencies]
"acme/http-kit" = "^1"
"#,
        )
        .unwrap();
        let empty_lock = Lockfile::default();

        let enforced = validate_frozen_manifest_requirements(&manifest, &empty_lock, None, true)
            .unwrap_err()
            .to_string();
        assert!(enforced.contains("acme/http-kit"));
        assert!(
            validate_frozen_manifest_requirements(&manifest, &empty_lock, None, false,).is_ok()
        );
    }

    #[test]
    fn split_key_accepts_org_name_and_keeps_nested_slashes_in_name() {
        assert_eq!(
            split_key("acme/http-kit").unwrap(),
            ("acme".to_string(), "http-kit".to_string())
        );
        // splitn(2): only the FIRST slash separates org from name.
        assert_eq!(
            split_key("acme/scoped/name").unwrap(),
            ("acme".to_string(), "scoped/name".to_string())
        );
    }

    #[test]
    fn split_key_rejects_missing_or_empty_halves() {
        for bad in ["noslash", "/name", "org/", "/"] {
            assert!(split_key(bad).is_err(), "`{bad}` must not split");
        }
    }

    fn ecos(list: &[Ecosystem]) -> BTreeSet<Ecosystem> {
        list.iter().copied().collect()
    }

    #[test]
    fn a_matching_ecosystem_installs_without_complaint() {
        assert!(
            ecosystem_mismatch(
                "acme/acme-clients-java",
                "acme-clients-java",
                Language::Java,
                Ecosystem::Jvm,
                &ecos(&[Ecosystem::Jvm]),
            )
            .is_none()
        );
    }

    #[test]
    fn a_wrong_language_package_is_refused_and_names_the_right_one() {
        // The core guard: a Java client in a Node project would sit in
        // zed_modules/ unread, so this must fail loudly and point at the
        // sibling that would work.
        let msg = ecosystem_mismatch(
            "acme/acme-clients-java",
            "acme-clients-java",
            Language::Java,
            Ecosystem::Jvm,
            &ecos(&[Ecosystem::Npm]),
        )
        .expect("a jvm package in an npm-only project must be refused");
        assert!(msg.contains("`jvm`"), "{msg}");
        assert!(msg.contains("`npm`"), "{msg}");
        assert!(msg.contains("acme/acme-clients-nodejs"), "{msg}");
        // The escape hatch is discoverable from the error itself.
        assert!(msg.contains("--allow-ecosystem-mismatch"), "{msg}");
    }

    #[test]
    fn a_polyglot_project_accepts_a_package_for_any_of_its_ecosystems() {
        // A Rust service with a TS frontend legitimately consumes either.
        let project = ecos(&[Ecosystem::Cargo, Ecosystem::Npm]);
        for (name, lang, eco) in [
            ("acme-clients-nodejs", Language::Nodejs, Ecosystem::Npm),
            ("acme-clients-rust", Language::Rust, Ecosystem::Cargo),
        ] {
            assert!(
                ecosystem_mismatch(&format!("acme/{name}"), name, lang, eco, &project).is_none(),
                "{name} must install in a cargo+npm project"
            );
        }
        // …but still not a third one it has no toolchain for.
        assert!(
            ecosystem_mismatch(
                "acme/acme-clients-golang",
                "acme-clients-golang",
                Language::Golang,
                Ecosystem::Gomod,
                &project,
            )
            .is_some()
        );
    }

    #[test]
    fn an_untagged_package_is_never_gated() {
        // Every package published before language tagging claims no ecosystem;
        // gating those would break existing installs everywhere.
        assert!(
            ecosystem_mismatch(
                "acme/http-kit",
                "http-kit",
                Language::Universal,
                Ecosystem::Universal,
                &ecos(&[Ecosystem::Npm]),
            )
            .is_none()
        );
    }

    #[test]
    fn a_project_with_no_recognizable_ecosystem_is_not_gated() {
        // Unverifiable is not the same as wrong: a fresh directory or a plain
        // Makefile project must still be able to install anything.
        assert!(
            ecosystem_mismatch(
                "acme/acme-clients-java",
                "acme-clients-java",
                Language::Java,
                Ecosystem::Jvm,
                &BTreeSet::new(),
            )
            .is_none()
        );
    }

    #[test]
    fn jvm_projects_are_offered_both_jvm_language_variants() {
        let msg = ecosystem_mismatch(
            "acme/acme-clients-nodejs",
            "acme-clients-nodejs",
            Language::Nodejs,
            Ecosystem::Npm,
            &ecos(&[Ecosystem::Jvm]),
        )
        .expect("npm package in a jvm project must be refused");
        assert!(msg.contains("acme/acme-clients-java"), "{msg}");
        assert!(msg.contains("acme/acme-clients-kotlin"), "{msg}");
    }

    #[test]
    fn a_package_not_following_the_suffix_convention_still_errors_without_suggestions() {
        // A single-language package whose name does not end in its language
        // cannot have siblings guessed, but must still be refused.
        let msg = ecosystem_mismatch(
            "acme/jackson-helpers",
            "jackson-helpers",
            Language::Java,
            Ecosystem::Jvm,
            &ecos(&[Ecosystem::Npm]),
        )
        .expect("still a mismatch");
        assert!(msg.contains("`jvm`"), "{msg}");
        assert!(!msg.contains("try instead"), "no basis to suggest: {msg}");
    }

    #[test]
    fn detect_adapter_recognizes_each_supported_toolchain() {
        let tmp = tempfile::tempdir().unwrap();
        for (marker, expected) in [
            ("package.json", Adapter::Node),
            ("go.mod", Adapter::Go),
            ("pyproject.toml", Adapter::Python),
            ("Cargo.toml", Adapter::Rust),
            ("pubspec.yaml", Adapter::Dart),
            ("pom.xml", Adapter::Java),
        ] {
            let dir = tmp.path().join(marker.replace('.', "_"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(marker), "").unwrap();
            assert_eq!(detect_adapter(&dir), expected, "marker {marker}");
        }
        // No marker at all must stay `None`, so a fresh project can still fall
        // through to each dependency's own declared adapter.
        let empty = tmp.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(detect_adapter(&empty), Adapter::None);
    }

    #[test]
    fn project_ecosystems_reads_the_real_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("package.json"), "{}").unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        let found = project_ecosystems(tmp.path());
        assert!(found.contains(&Ecosystem::Npm));
        assert!(found.contains(&Ecosystem::Cargo));
        assert!(!found.contains(&Ecosystem::Jvm));
    }

    #[test]
    fn a_bare_repo_name_routes_to_the_project_language() {
        // `zed add acme/acme-clients` in a Gradle project should reach the Java
        // package; the canonical token is tried before any synonym.
        let c = language_route_candidates("acme-clients", Some("java"));
        assert_eq!(c.first().map(String::as_str), Some("acme-clients-java"));

        let node = language_route_candidates("acme-clients", Some("node"));
        assert_eq!(
            node.first().map(String::as_str),
            Some("acme-clients-nodejs")
        );
        assert!(node.contains(&"acme-clients-ts".to_string()));

        let go = language_route_candidates("acme-clients", Some("go"));
        assert_eq!(go.first().map(String::as_str), Some("acme-clients-golang"));
    }

    #[test]
    fn a_misspelled_language_suffix_routes_to_its_synonyms() {
        // `-node` must reach `-nodejs` and vice versa, so a user who guesses
        // either spelling lands on whichever the author published.
        let from_short = language_route_candidates("acme-clients-node", None);
        assert!(
            from_short.contains(&"acme-clients-nodejs".to_string()),
            "{from_short:?}"
        );
        let from_long = language_route_candidates("acme-clients-nodejs", None);
        assert!(
            from_long.contains(&"acme-clients-node".to_string()),
            "{from_long:?}"
        );

        let go = language_route_candidates("acme-clients-go", None);
        assert!(go.contains(&"acme-clients-golang".to_string()), "{go:?}");
    }

    #[test]
    fn routing_never_suggests_the_name_that_was_asked_for() {
        // The caller already tried the exact name; repeating it would be a
        // wasted registry round-trip and a confusing message.
        for name in ["acme-clients", "acme-clients-java", "acme-clients-nodejs"] {
            let c = language_route_candidates(name, Some("java"));
            assert!(!c.contains(&name.to_string()), "{name} in {c:?}");
        }
    }

    #[test]
    fn routing_never_crosses_between_languages() {
        // A Java suffix must not produce Node candidates: that would install a
        // different client than the user asked for.
        let c = language_route_candidates("acme-clients-java", None);
        for candidate in &c {
            assert!(
                !candidate.contains("node") && !candidate.contains("golang"),
                "java routing produced {candidate}"
            );
        }
        // Java and Kotlin share an ecosystem but are different languages.
        assert!(!c.contains(&"acme-clients-kotlin".to_string()), "{c:?}");
    }

    #[test]
    fn a_bare_name_with_no_detectable_project_language_has_nothing_to_route_to() {
        assert!(language_route_candidates("acme-clients", None).is_empty());
        assert!(language_route_candidates("http-kit", Some("cobol")).is_empty());
    }
}
