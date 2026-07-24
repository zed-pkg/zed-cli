use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::manifest::{
    Manifest, PackageSection, PublishSection, RepositorySection, ScriptsSection,
};
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};
use zed_interfaces::registry::{PublishMeta, VersionMetadata};
use zed_interfaces::vcs::Vcs;
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
# Run by `zed test-local` inside a throwaway consumer project:
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

pub fn install(
    project: &Path,
    cfg: &Config,
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
) -> Result<InstallOutcome> {
    let adapter = match adapter {
        Adapter::Auto => detect_adapter(project),
        other => other,
    };
    let manifest = read_manifest(project)?;
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
    let mut installed = Vec::new();
    let mut shas = Vec::new();
    let mut jars: Vec<String> = Vec::new();
    for vm in resolved.values() {
        let pkg_dir = ensure_artifact(reg.as_ref(), &store, vm)?;
        let dest = modules.join(&vm.org).join(&vm.name);
        link_or_copy(&pkg_dir, &dest, mode)?;
        match adapter {
            Adapter::Node => {
                let node_dest = project
                    .join("node_modules")
                    .join(format!("@{}", vm.org))
                    .join(&vm.name);
                link_or_copy(&pkg_dir, &node_dest, mode)?;
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
// pack / publish / test-local

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

/// r2g-style pre-publish check (github.com/oresoftware/r2g): consume your
/// own artifact exactly the way an end user would, from a throwaway
/// file:// registry into a throwaway consumer project and store.
pub fn test_local(project: &Path, _cfg: &Config) -> Result<()> {
    let manifest = read_manifest(project)?;
    let tmp = tempfile::tempdir()?;
    let registry_dir = tmp.path().join("registry");
    let consumer_dir = tmp.path().join("consumer");
    let home_dir = tmp.path().join("home");
    fs::create_dir_all(&consumer_dir)?;

    let packed = pack::pack(project, &manifest, Some(&tmp.path().join("pack")))?;
    println!(
        "packed {} ({} files, {} excluded)",
        human_size(packed.size),
        packed.file_count,
        packed.excluded_count
    );
    let meta = build_publish_meta(&manifest, &packed, None);
    let file_registry = crate::registry::FileRegistry::new(registry_dir.clone());
    file_registry.publish(&meta, &packed.path, None)?;

    let mut dependencies = BTreeMap::new();
    dependencies.insert(
        manifest.full_name(),
        format!("={}", manifest.package.version),
    );
    let consumer_manifest = Manifest {
        package: PackageSection {
            org: "zed-local".to_string(),
            name: "consumer".to_string(),
            version: "0.0.0".to_string(),
            version_scheme: version::VersionScheme::Semver,
            description: None,
            license: None,
            repository: RepositorySection {
                vcs: Vcs::Git,
                url: "https://localhost/zed-local/consumer".to_string(),
            },
            keywords: Vec::new(),
        },
        dependencies,
        publish: PublishSection::default(),
        scripts: ScriptsSection::default(),
    };
    write_manifest(&consumer_dir, &consumer_manifest)?;

    let test_cfg = Config {
        registry: format!("file://{}", registry_dir.display()),
        home: home_dir,
        token: None,
    };
    install(
        &consumer_dir,
        &test_cfg,
        false,
        InstallMode::Symlink,
        Adapter::None,
    )?;

    let target = consumer_dir
        .join(MODULES_DIR)
        .join(&manifest.package.org)
        .join(&manifest.package.name);
    if !target.join(MANIFEST_FILE).exists() {
        bail!("installed package is missing {MANIFEST_FILE}; artifact is broken");
    }

    match &manifest.publish.smoke_test {
        Some(command) => {
            println!("running smoke_test: {command}");
            let status = Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&consumer_dir)
                .env("ZED_PKG_TEST_TARGET", &target)
                .status()?;
            if !status.success() {
                bail!("smoke_test failed with {status}");
            }
            println!("test-local passed: artifact installs and smoke_test succeeds");
        }
        None => {
            println!(
                "test-local passed: artifact installs cleanly \
                 (no publish.smoke_test configured; consider adding one)"
            );
        }
    }
    Ok(())
}

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
