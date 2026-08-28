//! Durable dependency installation for projects that do not yet have a Zed
//! manifest.
//!
//! The established `manifestless` module remains the explicit ephemeral path.
//! This module adds the normal managed-project path: infer the same conservative
//! root, write a deterministic consumer manifest, and delegate graph resolution
//! and materialization to the existing installer. If installation fails, the
//! exact generated manifest is removed (or the prior generated manifest is
//! restored) rather than leaving a half-adopted project.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use walkdir::{DirEntry, WalkDir};
use zed_interfaces::manifest::{
    Manifest, PackageSection, PublishSection, RepositorySection, ScriptsSection,
};
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::vcs::Vcs;
use zed_interfaces::version::{self, VersionScheme};
use zed_lock::{LockClass, LockGuard, LockManager, LockRequest};

use crate::cli::{Adapter, InstallMode};
use crate::config::{self, Config};
use crate::manifestless;
use crate::ops;
use crate::registry::registry_for;

const GENERATED_ORG: &str = "zed-local";
const GENERATED_VERSION: &str = "0.0.0";
const GENERATED_MARKER: &str = "zed-generated-consumer";
const GENERATED_DESCRIPTION: &str =
    "Local Zed dependency manifest; edit package metadata before publishing";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectSelection {
    root: PathBuf,
    has_manifest: bool,
}

/// Install dependencies while making a missing manifest durable by default.
///
/// `do_not_write_new_manifest` is the explicit compatibility/ephemeral path;
/// it delegates unchanged resolver, lockfile, store, adapter, and build-hook
/// behavior to the original manifestless implementation.
#[allow(clippy::too_many_arguments)]
pub fn install(
    requested_root: &Path,
    cfg: &Config,
    specs: &[String],
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    allow_build: bool,
    target: Option<&str>,
    do_not_write_new_manifest: bool,
    allow_ecosystem_mismatch: bool,
) -> Result<ops::InstallOutcome> {
    let permissions = ops::InstallPermissions {
        allow_build,
        ..ops::InstallPermissions::default()
    };
    install_with_permissions(
        requested_root,
        cfg,
        specs,
        frozen,
        mode,
        adapter,
        &permissions,
        target,
        do_not_write_new_manifest,
        allow_ecosystem_mismatch,
    )
}

/// Permission-aware form used by the CLI. Native package-manager consent,
/// package hook consent, and build consent remain independent through durable
/// first-manifest creation as well as the existing-manifest path.
#[allow(clippy::too_many_arguments)]
pub fn install_with_permissions(
    requested_root: &Path,
    cfg: &Config,
    specs: &[String],
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &ops::InstallPermissions,
    target: Option<&str>,
    do_not_write_new_manifest: bool,
    allow_ecosystem_mismatch: bool,
) -> Result<ops::InstallOutcome> {
    let initial = select_project(requested_root);

    if do_not_write_new_manifest {
        if initial.has_manifest {
            eprintln!(
                "note: --do-not-write-new-manifest has no effect because {} already exists in {}",
                MANIFEST_FILE,
                initial.root.display()
            );
        } else {
            eprintln!(
                "ephemeral install requested: {} will not be created in {}",
                MANIFEST_FILE,
                initial.root.display()
            );
        }
        return manifestless::install(
            requested_root,
            cfg,
            specs,
            frozen,
            mode,
            adapter,
            permissions.allow_build,
            permissions.allow_native_deps,
            permissions.allow_install_hooks,
            permissions.native_manager.as_deref(),
            target,
            true,
            allow_ecosystem_mismatch,
        );
    }

    if initial.has_manifest {
        if specs.is_empty() || !is_generated_manifest_path(&initial.root)? {
            return manifestless::install(
                requested_root,
                cfg,
                specs,
                frozen,
                mode,
                adapter,
                permissions.allow_build,
                permissions.allow_native_deps,
                permissions.allow_install_hooks,
                permissions.native_manager.as_deref(),
                target,
                false,
                allow_ecosystem_mismatch,
            );
        }

        // A generated consumer manifest may be visible before a second first-
        // install process reaches this function. Serialize that path too; do
        // not rely only on observing the file appear while already waiting.
        let _project_lock = lock_project_manifest(cfg, &initial.root)?;
        let selection = select_project(requested_root);
        if selection.has_manifest {
            return install_with_generated_manifest(
                &selection.root,
                cfg,
                specs,
                frozen,
                mode,
                adapter,
                permissions,
                target,
                allow_ecosystem_mismatch,
            );
        }

        return create_manifest_and_install(
            &selection.root,
            cfg,
            specs,
            frozen,
            mode,
            adapter,
            permissions,
            target,
            allow_ecosystem_mismatch,
        );
    }

    if specs.is_empty() {
        if frozen {
            bail!(
                "no {MANIFEST_FILE} exists and a lockfile cannot identify which packages were direct dependencies; re-run with --do-not-write-new-manifest for an explicit lock-only restore"
            );
        }
        bail!(
            "no {MANIFEST_FILE} and no package specs were supplied; pass one or more `org/name[@requirement]` operands"
        );
    }

    let _project_lock = lock_project_manifest(cfg, &initial.root)?;
    // Another invocation may have created a manifest while this process waited
    // for the project lock. Re-select under the lock before deciding whether to
    // create or merge.
    let selection = select_project(requested_root);
    if selection.has_manifest {
        return install_with_generated_manifest(
            &selection.root,
            cfg,
            specs,
            frozen,
            mode,
            adapter,
            permissions,
            target,
            allow_ecosystem_mismatch,
        );
    }

    create_manifest_and_install(
        &selection.root,
        cfg,
        specs,
        frozen,
        mode,
        adapter,
        permissions,
        target,
        allow_ecosystem_mismatch,
    )
}

/// Generated consumer manifests are deliberately non-publishable until a
/// person replaces the inferred identity and removes the marker. Installation
/// remains useful immediately; accidental registry publication fails closed.
pub fn ensure_publishable(project: &Path) -> Result<()> {
    let manifest = config::read_manifest(project)?;
    if is_non_publishable_generated(&manifest) {
        bail!(
            "{} is an auto-generated local consumer manifest and cannot be published; edit [package] identity/repository metadata and remove the `{GENERATED_MARKER}` keyword first",
            project.join(MANIFEST_FILE).display()
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_manifest_and_install(
    project: &Path,
    cfg: &Config,
    specs: &[String],
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &ops::InstallPermissions,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<ops::InstallOutcome> {
    let dependencies = resolve_direct_dependencies(cfg, specs)?;
    let inferred_target = target
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| ops::detect_target(project));
    let effective_adapter = match adapter {
        Adapter::Auto => ops::detect_adapter(project),
        explicit => explicit,
    };
    let manifest = generated_manifest(
        project,
        dependencies,
        inferred_target.as_deref(),
        effective_adapter,
    );
    // Validate the complete inferred package identity, dependency
    // requirements, repository metadata, and install values before touching
    // project state. Serialization alone does not run schema validation.
    manifest.validate()?;
    let manifest_text = manifest.to_toml_string()?;
    let manifest_path = project.join(MANIFEST_FILE);

    write_new_atomic(&manifest_path, manifest_text.as_bytes())?;
    eprintln!(
        "created {} with {} direct dependency/dependencies (target: {}, adapter: {})",
        manifest_path.display(),
        manifest.dependencies.len(),
        inferred_target.as_deref().unwrap_or("universal"),
        adapter_name(effective_adapter).unwrap_or("none")
    );

    match ops::install_with_permissions(
        project,
        cfg,
        frozen,
        mode,
        adapter,
        permissions,
        target,
        allow_ecosystem_mismatch,
    ) {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            if let Err(rollback) = remove_if_unchanged(&manifest_path, manifest_text.as_bytes()) {
                return Err(error.context(format!(
                    "installation failed and the generated manifest could not be rolled back: {rollback:#}"
                )));
            }
            Err(error)
        }
    }
}

/// Merge package operands into a recognized generated consumer manifest and
/// run the ordinary installer. This covers both concurrent first installs and
/// a later explicit install before the inferred package identity is authored.
/// Human-authored manifests retain the established `zed add` mutation rule.
#[allow(clippy::too_many_arguments)]
fn install_with_generated_manifest(
    project: &Path,
    cfg: &Config,
    specs: &[String],
    frozen: bool,
    mode: InstallMode,
    adapter: Adapter,
    permissions: &ops::InstallPermissions,
    target: Option<&str>,
    allow_ecosystem_mismatch: bool,
) -> Result<ops::InstallOutcome> {
    if specs.is_empty() {
        return ops::install_with_permissions(
            project,
            cfg,
            frozen,
            mode,
            adapter,
            permissions,
            target,
            allow_ecosystem_mismatch,
        );
    }

    let path = project.join(MANIFEST_FILE);
    let previous_text = fs::read_to_string(&path)
        .with_context(|| format!("reading generated {}", path.display()))?;
    let mut manifest = Manifest::parse(&previous_text)
        .with_context(|| format!("invalid generated {}", path.display()))?;
    if !is_generated_consumer(&manifest) {
        bail!(
            "package operands on `zed install` are not used to edit an existing {MANIFEST_FILE}; use `zed add <org>/<name>[@requirement]`"
        );
    }

    let additions = resolve_direct_dependencies(cfg, specs)?;
    for (key, requirement) in additions {
        if let Some(existing) = manifest.dependencies.get(&key)
            && existing != &requirement
        {
            bail!(
                "generated manifest already requires {key} as `{existing}`, which conflicts with `{requirement}`; use `zed add` to change an existing requirement"
            );
        }
        manifest.dependencies.insert(key, requirement);
    }
    if manifest.install.target.is_none() {
        manifest.install.target = target
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| ops::detect_target(project));
    }
    if manifest.install.adapter.is_none() {
        let effective = match adapter {
            Adapter::Auto => ops::detect_adapter(project),
            explicit => explicit,
        };
        manifest.install.adapter = adapter_name(effective).map(str::to_owned);
    }

    manifest.validate()?;
    let replacement_text = manifest.to_toml_string()?;
    replace_atomic(&path, replacement_text.as_bytes())?;
    match ops::install_with_permissions(
        project,
        cfg,
        frozen,
        mode,
        adapter,
        permissions,
        target,
        allow_ecosystem_mismatch,
    ) {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            if let Err(rollback) =
                restore_if_unchanged(&path, replacement_text.as_bytes(), previous_text.as_bytes())
            {
                return Err(error.context(format!(
                    "installation failed and the generated manifest update could not be rolled back: {rollback:#}"
                )));
            }
            Err(error)
        }
    }
}

fn resolve_direct_dependencies(cfg: &Config, specs: &[String]) -> Result<BTreeMap<String, String>> {
    let parsed = parse_requested_specs(specs)?;
    let registry = if parsed.values().any(Option::is_none) {
        Some(registry_for(&cfg.registry)?)
    } else {
        None
    };

    let mut dependencies = BTreeMap::new();
    for (key, explicit) in parsed {
        let requirement = match explicit {
            Some(requirement) => requirement,
            None => {
                let (org, name) = key
                    .split_once('/')
                    .expect("validated dependency identity contains slash");
                let package = registry
                    .as_ref()
                    .expect("registry exists when an unversioned spec exists")
                    .get_package(org, name)?;
                let latest = package
                    .latest
                    .with_context(|| format!("{key} has no published versions"))?;
                if version::parse_version(&latest).is_some() {
                    format!("^{latest}")
                } else {
                    latest
                }
            }
        };
        dependencies.insert(key, requirement);
    }
    Ok(dependencies)
}

fn parse_requested_specs(specs: &[String]) -> Result<BTreeMap<String, Option<String>>> {
    let mut requested = BTreeMap::new();
    for spec in specs {
        let (key, requirement) = split_dependency_spec(spec)?;
        if let Some(previous) = requested.insert(key.clone(), requirement.clone())
            && previous != requirement
        {
            bail!(
                "conflicting requirements for {key}: `{}` and `{}`",
                previous.as_deref().unwrap_or("latest"),
                requirement.as_deref().unwrap_or("latest")
            );
        }
    }
    Ok(requested)
}

fn split_dependency_spec(spec: &str) -> Result<(String, Option<String>)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        bail!("dependency spec cannot be empty");
    }
    let (key, requirement) = match trimmed.rsplit_once('@') {
        Some((key, requirement)) if key.contains('/') => {
            if requirement.trim().is_empty() {
                bail!("empty requirement in package spec `{spec}`");
            }
            (key, Some(requirement.trim().to_string()))
        }
        _ => (trimmed, None),
    };
    let (org, name) = key
        .split_once('/')
        .filter(|(org, name)| !org.is_empty() && !name.is_empty())
        .with_context(|| {
            format!("invalid package spec `{spec}` (expected org/name[@requirement])")
        })?;
    if name.contains('/') {
        bail!("invalid package spec `{spec}` (expected exactly org/name[@requirement])");
    }
    Ok((format!("{org}/{name}"), requirement))
}

fn generated_manifest(
    project: &Path,
    dependencies: BTreeMap<String, String>,
    target: Option<&str>,
    adapter: Adapter,
) -> Manifest {
    let name = project
        .file_name()
        .and_then(|name| name.to_str())
        .map(slugify)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string());
    let mut manifest = Manifest {
        package: PackageSection {
            org: GENERATED_ORG.to_string(),
            name: name.clone(),
            version: GENERATED_VERSION.to_string(),
            version_scheme: VersionScheme::Semver,
            description: Some(GENERATED_DESCRIPTION.to_string()),
            license: None,
            repository: RepositorySection {
                vcs: Vcs::Git,
                url: format!("https://localhost/{GENERATED_ORG}/{name}"),
            },
            keywords: vec![GENERATED_MARKER.to_string()],
            language: Default::default(),
            ecosystem: Default::default(),
        },
        workspace: None,
        dependencies,
        build_dependencies: BTreeMap::new(),
        native_dependencies: Default::default(),
        hooks: Default::default(),
        build: None,
        overrides: Default::default(),
        bin: BTreeMap::new(),
        publish: PublishSection::default(),
        scripts: ScriptsSection::default(),
        install: Default::default(),
        interop: Default::default(),
        targets: Default::default(),
    };
    manifest.install.target = target.map(str::to_owned);
    manifest.install.adapter = adapter_name(adapter).map(str::to_owned);
    manifest
}

fn is_generated_manifest_path(project: &Path) -> Result<bool> {
    let path = project.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let manifest = Manifest::parse(&text).with_context(|| format!("invalid {}", path.display()))?;
    Ok(is_generated_consumer(&manifest))
}

fn has_generated_marker(manifest: &Manifest) -> bool {
    manifest
        .package
        .keywords
        .iter()
        .any(|keyword| keyword == GENERATED_MARKER)
}

fn has_generated_placeholder_identity(manifest: &Manifest) -> bool {
    manifest.package.org == GENERATED_ORG
        && manifest.package.version == GENERATED_VERSION
        && manifest
            .package
            .repository
            .url
            .starts_with(&format!("https://localhost/{GENERATED_ORG}/"))
}

fn is_generated_consumer(manifest: &Manifest) -> bool {
    has_generated_marker(manifest) && has_generated_placeholder_identity(manifest)
}

fn is_non_publishable_generated(manifest: &Manifest) -> bool {
    has_generated_marker(manifest) || has_generated_placeholder_identity(manifest)
}

fn adapter_name(adapter: Adapter) -> Option<&'static str> {
    match adapter {
        Adapter::Auto => None,
        Adapter::None => Some("none"),
        Adapter::Node => Some("node"),
        Adapter::Java => Some("java"),
        Adapter::Go => Some("go"),
        Adapter::Python => Some("python"),
        Adapter::Rust => Some("rust"),
        Adapter::Dart => Some("dart"),
    }
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut pending_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_dash && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(ch.to_ascii_lowercase());
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

fn lock_project_manifest(cfg: &Config, project: &Path) -> Result<LockGuard> {
    let canonical = fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let key = hex::encode(hasher.finalize());
    let lock_dir = cfg.home.join("locks").join("projects");
    fs::create_dir_all(&lock_dir)
        .with_context(|| format!("creating project lock directory {}", lock_dir.display()))?;
    let lock_path = lock_dir.join(format!("{key}.manifest.lock"));
    LockManager::global()
        .acquire_blocking(
            LockRequest::exclusive(&lock_path)
                .operation(format!("project manifest {}", project.display()))
                .class(LockClass::Custom(5))
                .queue_same_process(),
        )
        .with_context(|| format!("locking project manifest {}", project.display()))
}

fn temporary_with_contents(path: &Path, contents: &[u8]) -> Result<NamedTempFile> {
    let parent = path.parent().context("manifest path has no parent")?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temporary manifest in {}", parent.display()))?;
    temporary
        .write_all(contents)
        .with_context(|| format!("writing temporary manifest for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("syncing temporary manifest for {}", path.display()))?;
    Ok(temporary)
}

fn write_new_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let temporary = temporary_with_contents(path, contents)?;
    temporary
        .persist_noclobber(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| {
            format!(
                "creating {}; it appeared concurrently or is not writable",
                path.display()
            )
        })
}

fn replace_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let temporary = temporary_with_contents(path, contents)?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("atomically replacing {}", path.display()))
}

fn remove_if_unchanged(path: &Path, expected: &[u8]) -> Result<()> {
    let current = match fs::read(path) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if current != expected {
        bail!(
            "{} changed during installation; refusing to remove another writer's content",
            path.display()
        );
    }
    fs::remove_file(path).with_context(|| format!("removing generated {}", path.display()))
}

fn restore_if_unchanged(path: &Path, expected: &[u8], previous: &[u8]) -> Result<()> {
    let current = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if current != expected {
        bail!(
            "{} changed during installation; refusing to overwrite another writer's content",
            path.display()
        );
    }
    replace_atomic(path, previous)
}

fn select_project(requested: &Path) -> ProjectSelection {
    if let Some(root) = manifest_ancestor(requested) {
        return ProjectSelection {
            root,
            has_manifest: true,
        };
    }
    if let Some(root) = native_or_lock_ancestor(requested) {
        return ProjectSelection {
            root,
            has_manifest: false,
        };
    }
    if let Some(root) = structure_ancestor(requested) {
        return ProjectSelection {
            root,
            has_manifest: false,
        };
    }
    ProjectSelection {
        root: unique_nested_project(requested).unwrap_or_else(|| requested.to_path_buf()),
        has_manifest: false,
    }
}

fn manifest_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(MANIFEST_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn native_or_lock_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(LOCKFILE_FILE).is_file() || ops::detect_native_manifest_target(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn structure_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if ops::detect_structure_target(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn nested_candidates(start: &Path, matches: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = WalkDir::new(start)
        .min_depth(1)
        .max_depth(4)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir() && matches(entry.path()))
        .map(|entry| entry.into_path())
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates
}

fn unique_nested_project(start: &Path) -> Option<PathBuf> {
    let mut authoritative = nested_candidates(start, |path| {
        path.join(LOCKFILE_FILE).is_file() || ops::detect_native_manifest_target(path).is_some()
    });
    let mut heuristic =
        nested_candidates(start, |path| ops::detect_structure_target(path).is_some());
    heuristic.retain(|candidate| !authoritative.iter().any(|root| candidate.starts_with(root)));
    authoritative.extend(heuristic);
    authoritative.sort();
    authoritative.dedup();
    (authoritative.len() == 1).then(|| authoritative.remove(0))
}

fn should_descend(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !name.starts_with('.')
        && !matches!(
            name.as_ref(),
            "node_modules" | "zed_modules" | "target" | "vendor" | "dist" | "build"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_manifest_is_deterministic_marked_and_installable() {
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("My Consumer_App");
        fs::create_dir_all(&nested).unwrap();
        let dependencies = BTreeMap::from([
            ("acme/http-kit".to_string(), "^1".to_string()),
            ("acme/log-kit".to_string(), "=2.0.0".to_string()),
        ]);
        let first = generated_manifest(&nested, dependencies.clone(), Some("node"), Adapter::Node);
        let second = generated_manifest(&nested, dependencies, Some("node"), Adapter::Node);
        assert_eq!(
            first.to_toml_string().unwrap(),
            second.to_toml_string().unwrap()
        );
        assert_eq!(first.package.name, "my-consumer-app");
        assert!(is_generated_consumer(&first));
        assert!(is_non_publishable_generated(&first));
        assert_eq!(first.install.target.as_deref(), Some("node"));
        assert_eq!(first.install.adapter.as_deref(), Some("node"));
        assert_eq!(
            first.dependencies.get("acme/http-kit").map(String::as_str),
            Some("^1")
        );
        first.validate().unwrap();
        Manifest::parse(&first.to_toml_string().unwrap()).unwrap();
    }

    #[test]
    fn explicit_none_adapter_is_durable() {
        let project = tempfile::tempdir().unwrap();
        let manifest = generated_manifest(
            project.path(),
            BTreeMap::from([("acme/http-kit".to_string(), "^1".to_string())]),
            None,
            Adapter::None,
        );
        assert_eq!(manifest.install.adapter.as_deref(), Some("none"));
        manifest.validate().unwrap();
    }

    #[test]
    fn package_operands_reject_invalid_and_conflicting_requirements() {
        assert!(split_dependency_spec("not-a-package").is_err());
        assert!(split_dependency_spec("acme/http-kit@").is_err());
        assert!(split_dependency_spec("a/b/c").is_err());
        assert!(
            parse_requested_specs(&[
                "acme/http-kit@^1".to_string(),
                "acme/http-kit@^2".to_string(),
            ])
            .is_err()
        );
    }

    #[test]
    fn generated_manifest_path_is_recognized_without_accepting_authored_files() {
        let project = tempfile::tempdir().unwrap();
        let generated = generated_manifest(
            project.path(),
            BTreeMap::from([("acme/http-kit".to_string(), "^1".to_string())]),
            None,
            Adapter::None,
        );
        fs::write(
            project.path().join(MANIFEST_FILE),
            generated.to_toml_string().unwrap(),
        )
        .unwrap();
        assert!(is_generated_manifest_path(project.path()).unwrap());

        let mut authored = generated;
        authored.package.keywords.clear();
        fs::write(
            project.path().join(MANIFEST_FILE),
            authored.to_toml_string().unwrap(),
        )
        .unwrap();
        assert!(!is_generated_manifest_path(project.path()).unwrap());
        assert!(is_non_publishable_generated(&authored));

        authored.package.org = "acme".to_string();
        authored.package.version = "1.0.0".to_string();
        authored.package.repository.url = "https://github.com/acme/project".to_string();
        assert!(!is_non_publishable_generated(&authored));
    }

    #[test]
    fn generated_marker_alone_still_blocks_publication() {
        let project = tempfile::tempdir().unwrap();
        let mut manifest = generated_manifest(
            project.path(),
            BTreeMap::from([("acme/http-kit".to_string(), "^1".to_string())]),
            None,
            Adapter::None,
        );
        manifest.package.org = "acme".to_string();
        manifest.package.version = "1.0.0".to_string();
        manifest.package.repository.url = "https://github.com/acme/project".to_string();
        assert!(has_generated_marker(&manifest));
        assert!(is_non_publishable_generated(&manifest));
        assert!(!is_generated_consumer(&manifest));
    }

    #[test]
    fn new_manifest_write_is_noclobber_and_failure_cleanup_is_exact() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join(MANIFEST_FILE);
        write_new_atomic(&path, b"first").unwrap();
        assert!(write_new_atomic(&path, b"second").is_err());
        assert!(remove_if_unchanged(&path, b"second").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        remove_if_unchanged(&path, b"first").unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn replacement_rollback_refuses_to_overwrite_external_changes() {
        let project = tempfile::tempdir().unwrap();
        let path = project.path().join(MANIFEST_FILE);
        fs::write(&path, b"before").unwrap();
        replace_atomic(&path, b"during").unwrap();
        fs::write(&path, b"external").unwrap();
        assert!(restore_if_unchanged(&path, b"during", b"before").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"external");
    }

    #[test]
    fn nested_native_project_selection_matches_the_manifestless_contract() {
        let project = tempfile::tempdir().unwrap();
        let root = project.path().join("apps/web");
        let invocation = root.join("src/deep");
        fs::create_dir_all(&invocation).unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();
        assert_eq!(select_project(&invocation).root, root);
    }
}
