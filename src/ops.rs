use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::manifest::{
    BuildSection, Manifest, PackageSection, PublishSection, RepositorySection, ScriptsSection,
    is_slug,
};
use zed_interfaces::paths::{BIN_DIR, LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR, current_platform};
use zed_interfaces::registry::{PublishMeta, VersionMetadata};
use zed_interfaces::vcs::Vcs;
use zed_interfaces::version::{self, Requirement};

use crate::cli::{Adapter, InstallMode};
use crate::config::{Config, Credentials, read_manifest, write_manifest};
use crate::pack::{self, PackResult};
use crate::registry::{Registry, registry_for};
use crate::store::{Store, human_size, require_sha256};
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

pub fn init(dir: &Path, org: Option<String>, name: Option<String>) -> Result<()> {
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
    let ignore_lines = format!("{MODULES_DIR}/\n.zed/\n");
    if gitignore.exists() {
        let current = fs::read_to_string(&gitignore)?;
        if !current.contains(MODULES_DIR) {
            fs::write(&gitignore, format!("{current}\n{ignore_lines}"))?;
        }
    } else {
        fs::write(&gitignore, ignore_lines)?;
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
    let mut info = WorkspaceInfo {
        root: root.to_path_buf(),
        members: BTreeMap::new(),
    };
    for pattern in globs {
        // Member globs are directory patterns like `packages/*`; expand one
        // path segment at a time so we never walk unrelated trees.
        let mut candidates = vec![root.to_path_buf()];
        for segment in pattern.split('/') {
            let mut next = Vec::new();
            for base in &candidates {
                if segment.contains('*') {
                    let Ok(glob) = globset::Glob::new(segment) else {
                        continue;
                    };
                    let matcher = glob.compile_matcher();
                    if let Ok(entries) = fs::read_dir(base) {
                        for entry in entries.flatten() {
                            let name = entry.file_name();
                            if entry.path().is_dir()
                                && matcher.is_match(Path::new(&name))
                                && !name.to_string_lossy().starts_with('.')
                            {
                                next.push(entry.path());
                            }
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
        for member_dir in candidates {
            if let Ok(member) = read_manifest(&member_dir) {
                info.members.insert(member.full_name(), member_dir);
            }
        }
    }
    info
}

// ---------------------------------------------------------------------------
// install

#[derive(Debug)]
pub struct InstallOutcome {
    pub installed: Vec<(String, String)>,
}

fn ensure_artifact(reg: &dyn Registry, store: &Store, vm: &VersionMetadata) -> Result<PathBuf> {
    validate_version_metadata(vm)?;
    if store.has(&vm.sha256) {
        return Ok(store.pkg_dir(&vm.sha256));
    }
    let cached = store.cached_artifact(&vm.sha256);
    if !cached.exists() {
        reg.download(vm, &cached)?;
    }
    store.add_artifact(&cached, &vm.sha256)
}

fn replace_dest(dest: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(dest) {
        if meta.file_type().is_dir() {
            fs::remove_dir_all(dest)?;
        } else {
            fs::remove_file(dest)?;
        }
    }
    Ok(())
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn link_or_copy(src: &Path, dest: &Path, mode: InstallMode) -> Result<()> {
    fs::create_dir_all(dest.parent().context("dest has parent")?)?;
    replace_dest(dest)?;
    match mode {
        InstallMode::Symlink => {
            #[cfg(unix)]
            std::os::unix::fs::symlink(src, dest)?;
            #[cfg(not(unix))]
            copy_dir(src, dest)?;
        }
        InstallMode::Copy => copy_dir(src, dest)?,
    }
    Ok(())
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

/// Infer the ecosystem from the files a project keeps at its root. Ordered so
/// the most specific marker wins when a repo carries several (a Next.js app
/// with a Dockerfile is still `node`).
fn detect_target(project: &Path) -> Option<String> {
    const MARKERS: &[(&str, &str)] = &[
        ("package.json", "node"),
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
        ("CMakeLists.txt", "cpp"),
    ];
    MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}

/// Pick the ecosystem adapter from what the project looks like: Node
/// projects resolve from node_modules/, JVM projects need a classpath,
/// Rust/others use zed_modules/ directly.
fn detect_adapter(project: &Path) -> Adapter {
    if project.join("package.json").exists() {
        Adapter::Node
    } else if project.join("pom.xml").exists()
        || project.join("build.gradle").exists()
        || project.join("build.gradle.kts").exists()
    {
        Adapter::Java
    } else {
        Adapter::None
    }
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
        allow_build,
        target,
    )
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
    allow_build: bool,
    target: Option<&str>,
) -> Result<InstallOutcome> {
    let adapter = match adapter {
        Adapter::Auto => detect_adapter(project),
        other => other,
    };
    let manifest = read_manifest(project)?;
    let resolved_target = resolve_target(project, &manifest, target);
    let reg = registry_for(&cfg.registry)?;
    let lock_path = project.join(LOCKFILE_FILE);

    let workspace = find_workspace(project);
    let mut workspace_links: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut resolved: BTreeMap<String, VersionMetadata> = BTreeMap::new();

    if frozen {
        let text = fs::read_to_string(&lock_path)
            .with_context(|| format!("--frozen requires {LOCKFILE_FILE}"))?;
        let lock = Lockfile::parse(&text)?;
        for (key, req_str) in &manifest.dependencies {
            let (org, name) = split_key(key)?;
            if workspace
                .as_ref()
                .is_some_and(|ws| ws.members.contains_key(key))
            {
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
        for locked in &lock.packages {
            if !is_slug(&locked.org) || !is_slug(&locked.name) {
                bail!(
                    "lockfile entry `{}/{}` has an invalid identity; refusing",
                    locked.org,
                    locked.name
                );
            }
            require_sha256(&locked.sha256)?;
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
            if let Some(ws) = &workspace
                && let Some(member_dir) = ws.members.get(&key)
            {
                if member_dir != project && !workspace_links.contains_key(&key) {
                    workspace_links.insert(key.clone(), member_dir.clone());
                    if let Ok(member_manifest) = read_manifest(member_dir) {
                        for (sub_key, sub_req) in member_manifest.dependencies {
                            let (sub_org, sub_name) = split_key(&sub_key)?;
                            queue.push_back((sub_org, sub_name, sub_req));
                        }
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
            resolved.insert(key, vm);
            if let Ok(sub_manifest) = read_manifest(&pkg_dir) {
                for (sub_key, sub_req) in sub_manifest.dependencies {
                    let (sub_org, sub_name) = split_key(&sub_key)?;
                    queue.push_back((sub_org, sub_name, sub_req));
                }
            }
        }
    }

    let modules_dir = manifest.modules_dir();
    let modules = project.join(modules_dir);
    let mut installed = Vec::new();
    let mut shas = Vec::new();
    let mut jars: Vec<String> = Vec::new();
    let mut bins: BTreeMap<String, PathBuf> = BTreeMap::new();
    for vm in resolved.values() {
        let pkg_dir = ensure_artifact(reg.as_ref(), store, vm)?;
        let pkg_manifest = read_manifest(&pkg_dir).ok();
        // A [build] step (the package's own, or the consumer's override)
        // swaps the link source from the pristine store entry to the
        // per-platform build-cache entry.
        let key = format!("{}/{}", vm.org, vm.name);
        let dep_build = pkg_manifest.as_ref().and_then(|m| m.build.as_ref());
        let link_src = match manifest.effective_build(&key, dep_build) {
            Some(build) => build_artifact(
                cfg,
                store,
                vm,
                &pkg_dir,
                pkg_manifest.as_ref(),
                &build,
                allow_build,
                false,
            )?,
            None => pkg_dir.clone(),
        };
        // Polyglot dependency: narrow the link source to this consumer's
        // language subtree, so a Python project gets `python/` at its import
        // root instead of a tree with the Node and Go sources beside it.
        // Single-language packages are unaffected (target_subdir -> None).
        let link_src = match pkg_manifest.as_ref() {
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
        match adapter {
            Adapter::Node => {
                let node_dest = project
                    .join("node_modules")
                    .join(format!("@{}", vm.org))
                    .join(&vm.name);
                link_or_copy(&link_src, &node_dest, mode)?;
            }
            Adapter::Java => {
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
            Adapter::Auto | Adapter::None => {}
        }
        installed.push((format!("{}/{}", vm.org, vm.name), vm.version.clone()));
        shas.push(vm.sha256.clone());
    }
    for (key, member_dir) in &workspace_links {
        let (org, name) = split_key(key)?;
        let dest = modules.join(&org).join(&name);
        link_or_copy(member_dir, &dest, InstallMode::Symlink)?;
        if let Ok(member_manifest) = read_manifest(member_dir) {
            for (bin_name, rel_target) in &member_manifest.bin {
                bins.insert(bin_name.clone(), dest.join(rel_target));
            }
        }
        installed.push((key.clone(), "workspace".to_string()));
    }
    hoist_bins(&modules, &bins, mode)?;
    if adapter == Adapter::Java {
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
    if adapter == Adapter::Node {
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
            source: cfg.registry.clone(),
        });
    }
    fs::write(&lock_path, lock.to_toml_string()?)?;
    store.record_project(project, shas)?;

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

// ---------------------------------------------------------------------------
// build hooks (zed-docs issue #5)

/// Build-cache key for a source artifact built with a given command: the
/// source sha256 plus a short hash of the command and declared outputs. Two
/// builds with different commands (e.g. a consumer override) never share an
/// entry.
fn build_cache_key(source_sha: &str, build: &BuildSection) -> String {
    let mut hasher = Sha256::new();
    hasher.update(build.command.as_bytes());
    hasher.update([0]);
    for out in &build.outputs {
        hasher.update(out.as_bytes());
        hasher.update([0]);
    }
    let cmd_hash = hex::encode(hasher.finalize());
    format!("{source_sha}-{}", &cmd_hash[..16])
}

/// Execute a dependency's build step, isolated from the immutable source
/// store (issue: source vs build caching):
///
///   store/<sha>/pkg  --copy-->  staging  --command-->  builds/<platform>/<sha>-<cmd>/pkg
///
/// Results cache per (sha256, platform, command); the staging dir gets the
/// package's `[build-dependencies]` installed into its own zed_modules
/// first. Builds run arbitrary package-author code, so they require
/// --allow-build; without it the pristine source is linked and a warning
/// explains how to opt in. `force` rebuilds even on a cache hit.
#[allow(clippy::too_many_arguments)]
fn build_artifact(
    cfg: &Config,
    store: &Store,
    vm: &VersionMetadata,
    pkg_dir: &Path,
    pkg_manifest: Option<&Manifest>,
    build: &BuildSection,
    allow_build: bool,
    force: bool,
) -> Result<PathBuf> {
    let key = format!("{}/{}", vm.org, vm.name);
    if !allow_build {
        eprintln!(
            "warning: {key} declares a [build] step; linking unbuilt source \
             (re-run with --allow-build or ZED_PKG_ALLOW_BUILD=1 to execute it)"
        );
        return Ok(pkg_dir.to_path_buf());
    }
    let platform = current_platform();
    let cache_key = build_cache_key(&vm.sha256, build);
    let built = store.build_pkg_dir(&platform, &cache_key);
    if built.is_dir() && !force {
        return Ok(built);
    }
    let _lock = store.build_lock(&platform, &cache_key)?;
    if built.is_dir() {
        if !force {
            return Ok(built);
        }
        let _ = fs::remove_dir_all(store.build_entry(&platform, &cache_key));
    }

    println!("building {key}@{} for {platform}...", vm.version);
    let staging = tempfile::tempdir()?;
    let work = staging.path().join("pkg");
    copy_dir(pkg_dir, &work)?;

    let build_deps = pkg_manifest
        .map(|m| m.build_dependencies.clone())
        .unwrap_or_default();
    if !build_deps.is_empty() {
        // Build deps live only in the staging dir for the duration of the
        // command; they are never linked into the consumer's project.
        let staging_manifest = Manifest {
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
            },
            dependencies: build_deps,
            build_dependencies: BTreeMap::new(),
            publish: PublishSection::default(),
            scripts: ScriptsSection::default(),
            bin: BTreeMap::new(),
            build: None,
            workspace: None,
            overrides: Default::default(),
            install: Default::default(),
            targets: Default::default(),
        };
        let deps_dir = staging.path().join("build-deps");
        fs::create_dir_all(&deps_dir)?;
        write_manifest(&deps_dir, &staging_manifest)?;
        install_locked(
            &deps_dir,
            cfg,
            store,
            false,
            InstallMode::Symlink,
            Adapter::None,
            false,
            // Build dependencies are toolchain, not the consumer's language:
            // take them whole rather than slicing them to a target.
            None,
        )?;
    }

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&build.command)
        .current_dir(&work)
        .env("ZED_BUILD_PLATFORM", &platform)
        .env("ZED_BUILD_SRC", &work);
    if !pkg_manifest
        .map(|m| m.build_dependencies.is_empty())
        .unwrap_or(true)
    {
        let bin_dir = staging
            .path()
            .join("build-deps")
            .join(MODULES_DIR)
            .join(BIN_DIR);
        let modules_dir = staging.path().join("build-deps").join(MODULES_DIR);
        let path_var = std::env::var("PATH").unwrap_or_default();
        command
            .env("PATH", format!("{}:{path_var}", bin_dir.display()))
            .env("ZED_BUILD_MODULES", &modules_dir);
    }
    let status = command
        .status()
        .with_context(|| format!("running [build] command for {key}"))?;
    if !status.success() {
        bail!(
            "[build] command for {key} failed with {status} \
             (override it via [overrides.build.\"{key}\"] in your manifest)"
        );
    }

    // Promote into the per-platform cache: either the whole staged tree or
    // just the declared outputs (plus the manifest so consumers can always
    // introspect what they linked).
    let entry = store.build_entry(&platform, &cache_key);
    let entry_parent = entry.parent().context("build entry has a parent")?;
    fs::create_dir_all(entry_parent)?;
    let promote_tmp = tempfile::tempdir_in(entry_parent)?;
    let promoted = promote_tmp.path().join("pkg");
    if build.outputs.is_empty() {
        copy_dir(&work, &promoted)?;
        // Staging-only artifacts never ship to consumers.
        let _ = fs::remove_dir_all(promoted.join(MODULES_DIR));
        let _ = fs::remove_file(promoted.join(LOCKFILE_FILE));
    } else {
        fs::create_dir_all(&promoted)?;
        for output in &build.outputs {
            let from = work.join(output);
            let to = promoted.join(output);
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
        let manifest_src = work.join(MANIFEST_FILE);
        if manifest_src.is_file() {
            fs::copy(&manifest_src, promoted.join(MANIFEST_FILE))?;
        }
    }
    fs::create_dir_all(&entry)?;
    let promote_path = promote_tmp.keep().join("pkg");
    match fs::rename(&promote_path, &built) {
        Ok(()) => {}
        Err(_) if built.is_dir() => {
            let _ = fs::remove_dir_all(promote_path.parent().unwrap_or(&promote_path));
        }
        Err(e) => {
            let _ = fs::remove_dir_all(promote_path.parent().unwrap_or(&promote_path));
            return Err(e.into());
        }
    }
    println!("built {key}@{} -> {}", vm.version, built.display());
    Ok(built)
}

/// `zed build [--force]` — run (or warm) the [build] steps of the locked
/// dependency graph on this machine. Running the command is itself consent
/// to execute package-author build code, like `install --allow-build`.
pub fn build_cmd(project: &Path, cfg: &Config, force: bool) -> Result<()> {
    let manifest = read_manifest(project)?;
    let reg = registry_for(&cfg.registry)?;
    let store = Store::new(&cfg.home);
    let _install_lock = store.install_lock()?;
    let lock_path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&lock_path)
        .with_context(|| format!("zed build needs {LOCKFILE_FILE}; run `zed install` first"))?;
    let lock = Lockfile::parse(&text)?;

    let mut built = 0usize;
    for locked in &lock.packages {
        let vm = reg.get_version(&locked.org, &locked.name, &locked.version)?;
        let pkg_dir = ensure_artifact(reg.as_ref(), &store, &vm)?;
        let pkg_manifest = read_manifest(&pkg_dir).ok();
        let key = format!("{}/{}", locked.org, locked.name);
        let dep_build = pkg_manifest.as_ref().and_then(|m| m.build.as_ref());
        let Some(build) = manifest.effective_build(&key, dep_build) else {
            continue;
        };
        let out = build_artifact(
            cfg,
            &store,
            &vm,
            &pkg_dir,
            pkg_manifest.as_ref(),
            &build,
            true,
            force,
        )?;
        println!("built {key}@{} -> {}", locked.version, out.display());
        built += 1;
    }
    if built == 0 {
        println!("no dependencies declare a build step");
    } else {
        println!(
            "built {built} package(s) (build cache: {})",
            store.builds_root().display()
        );
    }
    Ok(())
}

/// Hoist package-declared executables into `zed_modules/.bin/<name>` so
/// `zed run` and PATH-prepending wrappers find them without polluting the OS
/// PATH. Symlink installs get relative symlinks (they survive moving the
/// project); copy installs get real file copies so container image layers
/// stay self-contained.
fn hoist_bins(modules: &Path, bins: &BTreeMap<String, PathBuf>, mode: InstallMode) -> Result<()> {
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::metadata(target)?;
            let mut permissions = metadata.permissions();
            if permissions.mode() & 0o111 == 0 {
                permissions.set_mode(0o755);
                let _ = fs::set_permissions(target, permissions);
            }
        }
        let link = bin_dir.join(name);
        replace_dest(&link)?;
        match mode {
            InstallMode::Symlink => {
                #[cfg(unix)]
                {
                    let rel = pathdiff_relative(&bin_dir, target);
                    std::os::unix::fs::symlink(&rel, &link)?;
                }
                #[cfg(not(unix))]
                fs::copy(target, &link).map(|_| ())?;
            }
            InstallMode::Copy => {
                fs::copy(target, &link)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = fs::metadata(&link)?.permissions();
                    perms.set_mode(perms.mode() | 0o755);
                    fs::set_permissions(&link, perms)?;
                }
            }
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

/// `zed run <command>` — run a hoisted dependency binary (from
/// `<install.dir>/.bin`, default `zed_modules/.bin`) or any command, with that
/// directory prepended to PATH — npx-style, without polluting the OS PATH
/// (zed-docs issue #7). Returns the child's exit code.
pub fn run(project: &Path, command: &str, args: &[String]) -> Result<i32> {
    let modules_dir = project_modules_dir(project);
    let bin_dir = project.join(&modules_dir).join(BIN_DIR);
    let candidate = bin_dir.join(command);
    let mut paths: Vec<PathBuf> = vec![bin_dir.clone()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let new_path = std::env::join_paths(&paths).context("assembling PATH for zed run")?;
    // Prefer an exact hoisted bin by absolute path; otherwise fall through to
    // a normal PATH lookup (with .bin still prepended for the child's tools).
    let program: &Path = if candidate.exists() {
        &candidate
    } else {
        Path::new(command)
    };
    let status = Command::new(program)
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
    let token = cfg.resolve_token();
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

pub fn add(project: &Path, cfg: &Config, spec: &str) -> Result<()> {
    let (rest, req) = match spec.split_once('@') {
        Some((rest, req)) => (rest.to_string(), Some(req.to_string())),
        None => (spec.to_string(), None),
    };
    let (org, name) = split_key(&rest)?;
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
            let pkg = reg.get_package(&org, &name)?;
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
    write_manifest(project, &manifest)?;
    println!("added {org}/{name} = \"{req}\"");
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
    )?;
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
    write_manifest(project, &manifest)?;
    // Unlink from wherever install put it ([install].dir, default zed_modules),
    // otherwise a relocated tree keeps a stale copy of a removed dependency.
    let dest = project.join(manifest.modules_dir()).join(&org).join(&name);
    replace_dest(&dest)?;
    println!("removed {org}/{name}");
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
    )?;
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
        }
        return Ok(());
    }

    let reg = registry_for(&cfg.registry)?;
    let token = cfg.resolve_token();
    for package in &packages {
        let meta = build_publish_meta(&package.manifest, &package.packed, commit.clone());
        let identity = &package.manifest.package;
        // Multi-package releases cannot be a single HTTP transaction. Make
        // retries safe instead: an already-published byte-identical target is
        // accepted, while a same-version/different-hash target remains an
        // immutable-version error.
        if let Ok(existing) = reg.get_version(&identity.org, &identity.name, &identity.version) {
            if existing.sha256 == meta.sha256 {
                println!(
                    "already published {}/{}@{} with identical sha256; skipping",
                    identity.org, identity.name, identity.version
                );
                continue;
            }
            bail!(
                "{}/{}@{} already exists with sha256 {}; refusing to replace it with {}",
                identity.org,
                identity.name,
                identity.version,
                existing.sha256,
                meta.sha256
            );
        }
        let response = reg.publish(&meta, &package.packed.path, token.as_deref())?;
        println!(
            "published {}/{}@{} to {}",
            response.org, response.name, response.version, cfg.registry
        );
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
    let token = cfg.resolve_token();
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
    let token = cfg.resolve_token();
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
}
