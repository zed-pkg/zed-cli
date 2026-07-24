use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};
use zed_interfaces::registry::{PublishMeta, VersionMetadata};
use zed_interfaces::version::{self, Requirement};

use crate::cli::{Adapter, InstallMode};
use crate::config::{Config, Credentials, read_manifest, write_manifest};
use crate::pack::{self, PackResult};
use crate::registry::{Registry, registry_for};
use crate::store::{Store, human_size};
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
// install

#[derive(Debug)]
pub struct InstallOutcome {
    pub installed: Vec<(String, String)>,
}

fn ensure_artifact(reg: &dyn Registry, store: &Store, vm: &VersionMetadata) -> Result<PathBuf> {
    if store.has(&vm.sha256) {
        return Ok(store.pkg_dir(&vm.sha256));
    }
    let cached = store.cached_artifact(&vm.sha256);
    if !cached.exists() {
        reg.download(vm, &cached)?;
    }
    store.add_artifact(&cached, &vm.sha256)
}

/// Link a dependency's direct build-dependencies (source only) into a build
/// sandbox's `zed_modules/` so the build command can find them. They are
/// dropped before the built artifact is promoted (zed-docs issue #5).
fn link_build_deps(
    reg: &dyn Registry,
    store: &Store,
    deps: &BTreeMap<String, String>,
    work: &Path,
) -> Result<()> {
    for (dep_key, req_str) in deps {
        let (org, name) = split_key(dep_key)?;
        let req = Requirement::parse(req_str);
        let pkg = reg.get_package(&org, &name)?;
        let version = version::resolve(&req, &pkg.versions).with_context(|| {
            format!("build-dependency {dep_key}: no version satisfies `{req_str}`")
        })?;
        let vm = reg.get_version(&org, &name, version)?;
        let src = ensure_artifact(reg, store, &vm)?;
        let dest = work.join(MODULES_DIR).join(&org).join(&name);
        link_or_copy(&src, &dest, InstallMode::Symlink)?;
    }
    Ok(())
}

/// The directory a resolved dependency should be linked from: its source in
/// the store, or — if the dependency (or a consumer override) declares a
/// `[build]` step — the compiled output from the per-target build cache.
fn dep_link_source(
    reg: &dyn Registry,
    store: &Store,
    consumer: &Manifest,
    vm: &VersionMetadata,
    target: &str,
    force: bool,
) -> Result<PathBuf> {
    let pkg_dir = ensure_artifact(reg, store, vm)?;
    let dep_manifest = read_manifest(&pkg_dir).ok();
    let dep_build = dep_manifest.as_ref().and_then(|m| m.build.clone());
    let key = format!("{}/{}", vm.org, vm.name);
    let Some(build) = consumer.effective_build(&key, dep_build.as_ref()) else {
        return Ok(pkg_dir);
    };
    let build_deps = dep_manifest
        .map(|m| m.build_dependencies)
        .unwrap_or_default();
    crate::build::ensure_built(store, target, &vm.sha256, &pkg_dir, &build, force, |work| {
        link_build_deps(reg, store, &build_deps, work)
    })
}

/// Hoist a dependency's declared executables into `zed_modules/.bin/` so they
/// resolve on PATH under `zed run` (zed-docs issue #7). Shims point at the
/// project-local module directory, so they work in symlink and copy modes.
fn hoist_bins(
    project: &Path,
    module_dest: &Path,
    bins: &BTreeMap<String, String>,
    mode: InstallMode,
) -> Result<Vec<String>> {
    if bins.is_empty() {
        return Ok(Vec::new());
    }
    let bin_dir = project.join(MODULES_DIR).join(".bin");
    fs::create_dir_all(&bin_dir)?;
    let mut names = Vec::new();
    for (name, rel) in bins {
        let target = module_dest.join(rel);
        let shim = bin_dir.join(name);
        replace_dest(&shim)?;
        match mode {
            InstallMode::Symlink => {
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &shim)?;
                #[cfg(not(unix))]
                fs::copy(&target, &shim)?;
            }
            InstallMode::Copy => {
                fs::copy(&target, &shim)?;
                #[cfg(unix)]
                make_executable(&shim)?;
            }
        }
        names.push(name.clone());
    }
    Ok(names)
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
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

/// Expand `[workspace].members` glob patterns to member directories (each
/// containing a `.zpkg.toml`), relative to the workspace root. Heavy dirs
/// (`zed_modules`, `.git`, `node_modules`, `target`) are skipped.
fn expand_members(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
    use globset::{GlobBuilder, GlobSetBuilder};
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid workspace member glob `{pattern}`"))?,
        );
    }
    let set = builder.build()?;
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_string_lossy().as_ref(),
                MODULES_DIR | ".git" | "node_modules" | "target"
            )
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_dir() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if set.is_match(rel) && entry.path().join(MANIFEST_FILE).exists() {
            out.push(entry.path().to_path_buf());
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Resolve one external (non-member) dependency, honoring `--frozen` by
/// pinning to the workspace lock and verifying the artifact hash.
fn resolve_external(
    reg: &dyn Registry,
    key: &str,
    org: &str,
    name: &str,
    req: &Requirement,
    req_str: &str,
    frozen: bool,
    lock: Option<&Lockfile>,
) -> Result<VersionMetadata> {
    if frozen {
        let lock = lock.context("--frozen requires a lockfile")?;
        let entry = lock
            .find(org, name)
            .with_context(|| format!("--frozen: `{key}` is not in {LOCKFILE_FILE}"))?;
        if !req.matches(&entry.version) {
            bail!(
                "--frozen: lockfile pins {key}@{} which no longer satisfies `{req_str}`",
                entry.version
            );
        }
        let vm = reg.get_version(org, name, &entry.version)?;
        if vm.sha256 != entry.sha256 {
            bail!(
                "registry artifact for {key}@{} changed (lock {} vs registry {}); refusing",
                entry.version,
                entry.sha256,
                vm.sha256
            );
        }
        Ok(vm)
    } else {
        let pkg = reg.get_package(org, name)?;
        let version = version::resolve(req, &pkg.versions).with_context(|| {
            format!(
                "no version of {key} satisfies `{req_str}` (available: {})",
                pkg.versions.join(", ")
            )
        })?;
        reg.get_version(org, name, version)
    }
}

/// Monorepo workspace install (zed-docs issue #7): resolve every member's
/// dependencies against one store, path-link member→member deps for live
/// editing, and write a single root `.zpkg.lock`. zed's "one version per
/// package" rule holds workspace-wide.
fn install_workspace(
    root: &Path,
    root_manifest: &Manifest,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
) -> Result<InstallOutcome> {
    let store = Store::new(&cfg.home);
    let _install_lock = store.install_lock()?;
    let reg = registry_for(&cfg.registry)?;
    let target = crate::build::target_triple();

    let patterns = &root_manifest.workspace.as_ref().unwrap().members;
    let mut member_dirs = expand_members(root, patterns)?;
    // The root participates too, so its own dependencies get installed.
    member_dirs.insert(0, root.to_path_buf());
    member_dirs.dedup();

    // Index every workspace-local package by `org/name` for path linking.
    let mut index: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut manifests: BTreeMap<PathBuf, Manifest> = BTreeMap::new();
    for dir in &member_dirs {
        let m = read_manifest(dir)?;
        index.insert(m.full_name(), dir.clone());
        manifests.insert(dir.clone(), m);
    }

    let existing_lock = if frozen {
        let text = fs::read_to_string(root.join(LOCKFILE_FILE))
            .with_context(|| format!("--frozen requires {LOCKFILE_FILE} at the workspace root"))?;
        Some(Lockfile::parse(&text)?)
    } else {
        None
    };

    let mut union: BTreeMap<String, VersionMetadata> = BTreeMap::new();
    let mut all_shas: Vec<String> = Vec::new();
    let mut installed: Vec<(String, String)> = Vec::new();

    for dir in &member_dirs {
        let member_manifest = &manifests[dir];
        let mut member_external: BTreeMap<String, String> = BTreeMap::new();
        let mut linked_local: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let mut queue: VecDeque<(String, String, String)> = VecDeque::new();
        for (k, r) in &member_manifest.dependencies {
            let (o, n) = split_key(k)?;
            queue.push_back((o, n, r.clone()));
        }
        while let Some((org, name, req_str)) = queue.pop_front() {
            let key = format!("{org}/{name}");
            let req = Requirement::parse(&req_str);

            if let Some(local_dir) = index.get(&key) {
                if local_dir == dir || !linked_local.insert(key.clone()) {
                    continue;
                }
                // Live path link to the member's source (no build/registry).
                let dest = dir.join(MODULES_DIR).join(&org).join(&name);
                link_or_copy(local_dir, &dest, mode)?;
                let bins = manifests
                    .get(local_dir)
                    .map(|m| m.bin.clone())
                    .unwrap_or_default();
                hoist_bins(dir, &dest, &bins, mode)?;
                if let Some(lm) = manifests.get(local_dir) {
                    for (k, r) in &lm.dependencies {
                        let (o, n) = split_key(k)?;
                        queue.push_back((o, n, r.clone()));
                    }
                }
                installed.push((key, "workspace".to_string()));
                continue;
            }

            if let Some(existing) = member_external.get(&key) {
                if req.matches(existing) {
                    continue;
                }
                bail!(
                    "version conflict for {key} in member {}: resolved {existing} but `{req_str}` also required",
                    dir.display()
                );
            }
            let vm = resolve_external(
                reg.as_ref(),
                &key,
                &org,
                &name,
                &req,
                &req_str,
                frozen,
                existing_lock.as_ref(),
            )?;
            let link_src =
                dep_link_source(reg.as_ref(), &store, member_manifest, &vm, &target, false)?;
            let dest = dir.join(MODULES_DIR).join(&org).join(&name);
            link_or_copy(&link_src, &dest, mode)?;
            let bins = read_manifest(&store.pkg_dir(&vm.sha256))
                .map(|m| m.bin)
                .unwrap_or_default();
            hoist_bins(dir, &dest, &bins, mode)?;
            if let Ok(sub) = read_manifest(&store.pkg_dir(&vm.sha256)) {
                for (k, r) in sub.dependencies {
                    let (o, n) = split_key(&k)?;
                    queue.push_back((o, n, r));
                }
            }
            if let Some(prev) = union.get(&key) {
                if prev.version != vm.version {
                    bail!(
                        "workspace installs one version per package, but {key} resolves to both {} and {}",
                        prev.version,
                        vm.version
                    );
                }
            } else {
                union.insert(key.clone(), vm.clone());
                installed.push((key.clone(), vm.version.clone()));
            }
            all_shas.push(vm.sha256.clone());
            member_external.insert(key, vm.version.clone());
        }
    }

    let mut lock = Lockfile::default();
    for vm in union.values() {
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
    fs::write(root.join(LOCKFILE_FILE), lock.to_toml_string()?)?;
    all_shas.sort();
    all_shas.dedup();
    store.record_project(root, all_shas)?;

    println!(
        "workspace: {} member(s), {} external package(s) in one {LOCKFILE_FILE}",
        member_dirs.len(),
        union.len()
    );
    Ok(InstallOutcome { installed })
}

pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
) -> Result<InstallOutcome> {
    let manifest = read_manifest(project)?;
    if manifest.is_workspace_root() {
        return install_workspace(project, &manifest, cfg, frozen, mode);
    }
    let adapter = match adapter {
        Adapter::Auto => detect_adapter(project),
        other => other,
    };
    let reg = registry_for(&cfg.registry)?;
    let store = Store::new(&cfg.home);
    // Serialize against concurrent `zed install` processes (other terminals,
    // parallel CI runners) writing the store, refs.json, and lockfile.
    let _install_lock = store.install_lock()?;
    let lock_path = project.join(LOCKFILE_FILE);

    let mut resolved: BTreeMap<String, VersionMetadata> = BTreeMap::new();

    if frozen {
        let text = fs::read_to_string(&lock_path)
            .with_context(|| format!("--frozen requires {LOCKFILE_FILE}"))?;
        let lock = Lockfile::parse(&text)?;
        for (key, req_str) in &manifest.dependencies {
            let (org, name) = split_key(key)?;
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
            let version = version::resolve(&req, &pkg.versions).with_context(|| {
                format!(
                    "no version of {key} satisfies `{req_str}` (available: {})",
                    pkg.versions.join(", ")
                )
            })?;
            let vm = reg.get_version(&org, &name, version)?;
            let pkg_dir = ensure_artifact(reg.as_ref(), &store, &vm)?;
            resolved.insert(key, vm);
            if let Ok(sub_manifest) = read_manifest(&pkg_dir) {
                for (sub_key, sub_req) in sub_manifest.dependencies {
                    let (sub_org, sub_name) = split_key(&sub_key)?;
                    queue.push_back((sub_org, sub_name, sub_req));
                }
            }
        }
    }

    let modules = project.join(MODULES_DIR);
    let target = crate::build::target_triple();
    let mut installed = Vec::new();
    let mut shas = Vec::new();
    let mut jars: Vec<String> = Vec::new();
    let mut hoisted_bins: Vec<String> = Vec::new();
    for vm in resolved.values() {
        // Source, or compiled output if the package declares a build step.
        let link_src = dep_link_source(reg.as_ref(), &store, &manifest, vm, &target, false)?;
        let dest = modules.join(&vm.org).join(&vm.name);
        link_or_copy(&link_src, &dest, mode)?;
        // Expose any executables the package declares (zed-docs issue #7).
        let bins = read_manifest(&store.pkg_dir(&vm.sha256))
            .map(|m| m.bin)
            .unwrap_or_default();
        hoisted_bins.extend(hoist_bins(project, &dest, &bins, mode)?);
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
    if !hoisted_bins.is_empty() {
        hoisted_bins.sort();
        println!(
            "{} bin(s) in {MODULES_DIR}/.bin/ ({}); run with `zed run <name>`",
            hoisted_bins.len(),
            hoisted_bins.join(", ")
        );
    }
    println!(
        "{} package(s) in {MODULES_DIR}/ ({})",
        installed.len(),
        match mode {
            InstallMode::Symlink => "symlinked from the global store",
            InstallMode::Copy => "copied for container-safe layers",
        }
    );
    Ok(InstallOutcome { installed })
}

// ---------------------------------------------------------------------------
// build

/// Materialize the build cache for the locked dependency graph on a target
/// (zed-docs issue #5). Runs each dependency's (or consumer-overridden) build
/// step, caching per `(target, source sha, command)`. `--target` warms a
/// specific triple's cache; `--force` rebuilds even on a cache hit.
pub fn build(project: &Path, cfg: &Config, target: Option<String>, force: bool) -> Result<()> {
    let manifest = read_manifest(project)?;
    let reg = registry_for(&cfg.registry)?;
    let store = Store::new(&cfg.home);
    let _install_lock = store.install_lock()?;
    let lock_path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&lock_path)
        .with_context(|| format!("zed build needs {LOCKFILE_FILE}; run `zed install` first"))?;
    let lock = Lockfile::parse(&text)?;
    let target = target.unwrap_or_else(crate::build::target_triple);

    let mut built = 0usize;
    for locked in &lock.packages {
        let vm = reg.get_version(&locked.org, &locked.name, &locked.version)?;
        let pkg_dir = ensure_artifact(reg.as_ref(), &store, &vm)?;
        let dep_manifest = read_manifest(&pkg_dir).ok();
        let dep_build = dep_manifest.as_ref().and_then(|m| m.build.clone());
        let key = format!("{}/{}", locked.org, locked.name);
        let Some(build_step) = manifest.effective_build(&key, dep_build.as_ref()) else {
            continue;
        };
        let build_deps = dep_manifest
            .map(|m| m.build_dependencies)
            .unwrap_or_default();
        let out = crate::build::ensure_built(
            &store,
            &target,
            &vm.sha256,
            &pkg_dir,
            &build_step,
            force,
            |work| link_build_deps(reg.as_ref(), &store, &build_deps, work),
        )?;
        println!("built {key}@{} -> {}", locked.version, out.display());
        built += 1;
    }
    if built == 0 {
        println!("no dependencies declare a build step for target {target}");
    } else {
        println!(
            "built {built} package(s) for {target} (build cache: {})",
            store.build_root().display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// run

/// Run a hoisted dependency binary (or any command) with `zed_modules/.bin`
/// prepended to `PATH`, so a project's tools resolve to the versions it
/// installed without polluting the global `PATH` (zed-docs issue #7). Returns
/// the child's exit code.
pub fn run(project: &Path, command: &str, args: &[String]) -> Result<i32> {
    let bin_dir = project.join(MODULES_DIR).join(".bin");
    let mut paths: Vec<PathBuf> = vec![bin_dir];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let new_path = std::env::join_paths(&paths).context("assembling PATH for zed run")?;
    // Prefer an exact hoisted bin by absolute path; otherwise fall through to a
    // normal PATH lookup (with .bin still prepended for the child's own tools).
    let direct = paths[0].join(command);
    let program = if direct.exists() {
        direct
    } else {
        PathBuf::from(command)
    };
    let status = Command::new(&program)
        .args(args)
        .env("PATH", &new_path)
        .current_dir(project)
        .status()
        .with_context(|| {
            format!(
                "failed to run `{command}` \
                 (not a hoisted bin in {MODULES_DIR}/.bin/ nor on PATH?)"
            )
        })?;
    Ok(status.code().unwrap_or(1))
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
    install(project, cfg, false, InstallMode::Symlink, Adapter::None)?;
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
    let dest = project.join(MODULES_DIR).join(&org).join(&name);
    replace_dest(&dest)?;
    println!("removed {org}/{name}");
    install(project, cfg, false, InstallMode::Symlink, Adapter::None)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// pack / publish

pub fn pack_cmd(project: &Path, out: Option<&Path>) -> Result<PackResult> {
    let manifest = read_manifest(project)?;
    let result = pack::pack(project, &manifest, out)?;
    println!("packed {}", result.path.display());
    println!(
        "  sha256 {}\n  size {} ({} files, {} excluded by publish rules)",
        result.sha256,
        human_size(result.size),
        result.file_count,
        result.excluded_count
    );
    Ok(result)
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

    let packed = pack_cmd(project, None)?;
    let meta = build_publish_meta(&manifest, &packed, commit);

    if dry_run {
        println!(
            "dry run: would publish {}@{} (tag {}, sha256 {}) to {}",
            manifest.full_name(),
            manifest.package.version,
            meta.vcs_tag,
            meta.sha256,
            cfg.registry
        );
        return Ok(());
    }

    let reg = registry_for(&cfg.registry)?;
    let token = cfg.resolve_token();
    let response = reg.publish(&meta, &packed.path, token.as_deref())?;
    println!(
        "published {}/{}@{} to {}",
        response.org, response.name, response.version, cfg.registry
    );
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
            "build  {}  ({})",
            store.build_root().display(),
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

/// Parse a coarse age like `90d`, `2w`, `12h` (bare number = days).
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
    Ok(std::time::Duration::from_secs(n * secs))
}

/// `zed gc`: least-recently-used garbage collection of the global caches by
/// access time (zed-docs issue #7).
pub fn gc(cfg: &Config, older_than: &str, dry_run: bool) -> Result<()> {
    let store = Store::new(&cfg.home);
    let _install_lock = store.install_lock()?;
    let age = parse_age(older_than)?;
    let report = store.gc(age, dry_run)?;
    println!(
        "gc: {} {} across {} entr{} not accessed in {older_than}",
        if report.dry_run {
            "would reclaim"
        } else {
            "reclaimed"
        },
        human_size(report.freed),
        report.removed,
        if report.removed == 1 { "y" } else { "ies" },
    );
    Ok(())
}
