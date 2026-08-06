//! Git-submodule interoperability for Zed projects.
//!
//! Git remains a compatible transport: `zed install --git-submodules` runs
//! the ordinary recursive submodule sync/update before package resolution.
//! `zed overtake --git-submodules` makes Zed authoritative for dependency and
//! reproducibility metadata while retaining `.gitmodules` as a reversible
//! transport mirror.

mod cli;
mod git;
mod lock;

#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code, unused_imports))]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::manifest::{Manifest, WorkspaceSection};
use zed_interfaces::paths::MANIFEST_FILE;

use crate::cli::{Adapter, InstallMode};
use crate::config::{Config, read_manifest};
use crate::transaction::ProjectTransaction;
use git::{
    checked_git, collect_workspace_members, configured_submodules, exact_requirement,
    generated_consumer_manifest, verify_checkout, verify_gitmodules_committed,
    warn_on_repository_mismatch,
};

const MAX_INTEROP_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

pub use cli::{OvertakeArgs, augment_root_command, dispatch};
pub(crate) use lock::{preflight_mutation, prepare_install, refresh_lock_extensions};

#[derive(Debug)]
pub struct OvertakeReport {
    pub project: PathBuf,
    pub adopted: usize,
}

fn has_gitmodules_entry(candidate: &Path) -> bool {
    match fs::symlink_metadata(candidate.join(".gitmodules")) {
        Ok(_) => true,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

/// Find the nearest Git-submodule superproject at or above `requested`.
pub fn find_root(requested: &Path) -> Option<PathBuf> {
    requested
        .ancestors()
        .find(|candidate| has_gitmodules_entry(candidate))
        .map(Path::to_path_buf)
}

/// Whether the nearest Zed manifest explicitly opts into consuming the
/// checkout's `.gitmodules` transport metadata. The shared manifest contract
/// owns this field; reading it as TOML here keeps older CLI builds able to
/// diagnose the opt-in while the interfaces dependency rolls forward.
pub fn manifest_consumes_gitmodules(requested: &Path) -> Result<bool> {
    let manifest = requested
        .ancestors()
        .map(|candidate| candidate.join(MANIFEST_FILE))
        .find(|candidate| candidate.exists());
    let Some(manifest) = manifest else {
        return Ok(false);
    };
    Ok(manifest_gitmodules_consumption_from_file(&manifest)?.unwrap_or(false))
}

pub(crate) fn manifest_gitmodules_consumption(project: &Path) -> Result<Option<bool>> {
    let manifest = project.join(MANIFEST_FILE);
    if !manifest.exists() {
        return Ok(None);
    }
    manifest_gitmodules_consumption_from_file(&manifest)
}

fn manifest_gitmodules_consumption_from_file(manifest: &Path) -> Result<Option<bool>> {
    let metadata = fs::symlink_metadata(&manifest)
        .with_context(|| format!("inspecting {}", manifest.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "{} must be a regular file before Git-submodule consumption can be enabled",
            manifest.display()
        );
    }
    if metadata.len() > MAX_INTEROP_MANIFEST_BYTES {
        bail!(
            "{} exceeds the {}-byte Git-interop inspection limit",
            manifest.display(),
            MAX_INTEROP_MANIFEST_BYTES
        );
    }
    let document: toml::Value = toml::from_str(
        &fs::read_to_string(&manifest)
            .with_context(|| format!("reading {}", manifest.display()))?,
    )
    .with_context(|| format!("parsing {}", manifest.display()))?;
    let value = document
        .get("interop")
        .and_then(|value| value.get("git"))
        .and_then(|value| value.get("consume_gitmodules"));
    match value {
        None => Ok(None),
        Some(toml::Value::Boolean(value)) => Ok(Some(*value)),
        Some(_) => bail!(
            "[interop.git].consume_gitmodules in {} must be a boolean",
            manifest.display()
        ),
    }
}

pub(crate) fn set_manifest_consumes_gitmodules(
    manifest_text: &str,
    consumes_gitmodules: bool,
) -> Result<String> {
    let mut document: toml::Value = toml::from_str(manifest_text)
        .context("parsing manifest before updating Git interop declaration")?;
    let root = document
        .as_table_mut()
        .context("manifest root must be a TOML table")?;
    let interop = root
        .entry("interop")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("[interop] must be a TOML table")?;
    let git = interop
        .entry("git")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .context("[interop.git] must be a TOML table")?;
    git.insert(
        "consume_gitmodules".to_string(),
        toml::Value::Boolean(consumes_gitmodules),
    );
    toml::to_string_pretty(&document).context("serializing manifest with Git interop declaration")
}

fn verify_gitmodules_worktree_regular(project: &Path) -> Result<()> {
    let path = project.join(".gitmodules");
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspecting Git submodule metadata {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "{} must be a regular file; refusing mutable or indirect Git submodule metadata",
            path.display()
        );
    }
    Ok(())
}

fn validate_gitmodules_index(text: &str) -> Result<()> {
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (metadata, path) = line
            .split_once('\t')
            .with_context(|| format!("unrecognized Git index record for .gitmodules: `{line}`"))?;
        if path != ".gitmodules" {
            bail!("unexpected Git index path while validating .gitmodules: `{path}`");
        }
        let mut fields = metadata.split_whitespace();
        let mode = fields.next().unwrap_or_default();
        let object = fields.next().unwrap_or_default();
        let stage = fields.next().unwrap_or_default();
        if !matches!(mode, "100644" | "100755")
            || object.is_empty()
            || stage != "0"
            || fields.next().is_some()
        {
            bail!(".gitmodules must be a stage-zero regular Git blob, not index record `{line}`");
        }
    }
    Ok(())
}

fn verify_gitmodules_index_regular(project: &Path) -> Result<()> {
    let output = checked_git(project, &["ls-files", "--stage", "--", ".gitmodules"])
        .context("reading .gitmodules mode from the Git index")?;
    let text = String::from_utf8(output.stdout).context("Git index output is not UTF-8")?;
    validate_gitmodules_index(&text)
}

/// Reject indirect `.gitmodules` metadata before any parser, sync, lock, pack,
/// or publish operation consumes it. An untracked regular file remains usable
/// for cooperative Git workflows; once indexed, it must be a regular blob.
pub(crate) fn preflight_gitmodules_metadata(requested: &Path) -> Result<()> {
    let Some(root) = find_root(requested) else {
        return Ok(());
    };
    verify_gitmodules_worktree_regular(&root)?;
    verify_gitmodules_index_regular(&root)
}

/// Canonical Git-submodule metadata shared by takeover, frozen replay, and
/// package publication. Keeping parsing and checkout verification in this
/// subsystem prevents pack/publish policy from drifting from install policy.
#[derive(Debug)]
pub(crate) struct PackSubmodules {
    root: PathBuf,
    canonical_root: PathBuf,
    paths: Vec<String>,
}

impl PackSubmodules {
    pub(crate) fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = &str> {
        self.paths.iter().map(String::as_str)
    }

    pub(crate) fn verify(&self, included: &[String]) -> Result<()> {
        if included.is_empty() {
            return Ok(());
        }
        checked_git(&self.root, &["cat-file", "-e", "HEAD:.gitmodules"])
            .context("packing submodule source requires .gitmodules committed at HEAD")?;
        verify_gitmodules_committed(&self.root)?;

        for relative in included {
            let configured_child = self.canonical_root.join(relative);
            let marker = configured_child.join(".git");
            let marker_metadata = fs::symlink_metadata(&marker).with_context(|| {
                format!(
                    "included submodule `{relative}` is not initialized; run `zed install --git-submodules` before packing"
                )
            })?;
            if marker_metadata.file_type().is_symlink() {
                bail!(
                    "included submodule `{relative}` has a symlinked .git control path; refusing to package it"
                );
            }

            let child = fs::canonicalize(&configured_child)
                .with_context(|| format!("canonicalizing included submodule `{relative}`"))?;
            if !child.starts_with(&self.canonical_root) {
                bail!(
                    "included submodule `{relative}` resolves outside superproject {}: {}",
                    self.root.display(),
                    child.display()
                );
            }
            verify_checkout(&self.root, relative, &child).with_context(|| {
                format!("verifying included submodule `{relative}` for packing")
            })?;
        }
        Ok(())
    }
}

pub(crate) fn pack_submodules(requested: &Path) -> Result<Option<PackSubmodules>> {
    let Some(root) = find_root(requested) else {
        return Ok(None);
    };
    preflight_gitmodules_metadata(&root)?;
    let paths = configured_submodules(&root)?
        .into_iter()
        .map(|module| module.path)
        .collect();
    let canonical_root = fs::canonicalize(&root)
        .with_context(|| format!("canonicalizing Git superproject {}", root.display()))?;
    Ok(Some(PackSubmodules {
        root,
        canonical_root,
        paths,
    }))
}

/// Synchronize URLs and initialize/update every configured submodule,
/// recursively, at the exact gitlink commit selected by the superproject.
pub fn sync(requested: &Path) -> Result<usize> {
    let Some(project) = find_root(requested) else {
        eprintln!(
            "git-submodule mode enabled; no .gitmodules found at or above {}",
            requested.display()
        );
        return Ok(0);
    };
    sync_root(&project)
}

fn sync_root(project: &Path) -> Result<usize> {
    preflight_gitmodules_metadata(project)?;
    let configured = configured_submodules(project)?;
    if configured.is_empty() {
        eprintln!(
            "{} contains no configured Git submodules",
            project.display()
        );
        return Ok(0);
    }

    checked_git(project, &["submodule", "sync", "--recursive"])
        .context("synchronizing Git submodule URLs")?;
    // An explicit update strategy prevents a malicious/custom `update = !...`
    // procedure from being selected through repository configuration.
    checked_git(
        project,
        &["submodule", "update", "--init", "--recursive", "--checkout"],
    )
    .context("initializing Git submodules")?;
    println!(
        "synchronized {} Git submodule(s) in {}",
        configured.len(),
        project.display()
    );
    Ok(configured.len())
}

fn submodule_manifest_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => bail!("{} exists but is not a regular file", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

/// Import every top-level `.gitmodules` entry that contains a valid Zed
/// package. Entries without a `.zpkg.toml` remain under Git authority, so mixed
/// Zed/non-Zed submodule repositories can migrate incrementally. The root
/// manifest becomes the graph authority for adopted packages while Git metadata
/// remains a reversible clone/update transport.
pub fn overtake(requested: &Path, cfg: &Config) -> Result<OvertakeReport> {
    let project = find_root(requested).with_context(|| {
        format!(
            "`zed overtake --git-submodules` requires .gitmodules at or above {}",
            requested.display()
        )
    })?;
    // Takeover is an authority migration, not merely a convenience checkout.
    // Refuse to fetch from working-tree-only or dirty transport metadata.
    preflight_gitmodules_metadata(&project)?;
    checked_git(&project, &["cat-file", "-e", "HEAD:.gitmodules"])
        .context("takeover requires .gitmodules to be committed at superproject HEAD")?;
    verify_gitmodules_committed(&project)?;
    sync_root(&project)?;
    let modules = configured_submodules(&project)?;
    if modules.is_empty() {
        bail!(
            "{} contains no Git submodules to overtake",
            project.display()
        );
    }

    let canonical_project = fs::canonicalize(&project)
        .with_context(|| format!("canonicalizing superproject {}", project.display()))?;
    let mut imported = Vec::with_capacity(modules.len());
    let mut skipped = Vec::new();
    let mut seen_packages = BTreeSet::new();
    for module in &modules {
        let configured_child = project.join(&module.path);
        if !configured_child.is_dir() {
            bail!(
                "submodule `{}` is not initialized at {}; run `zed install --git-submodules`",
                module.name,
                configured_child.display()
            );
        }
        let child = fs::canonicalize(&configured_child).with_context(|| {
            format!(
                "canonicalizing submodule `{}` at {}",
                module.name,
                configured_child.display()
            )
        })?;
        if !child.starts_with(&canonical_project) {
            bail!(
                "submodule `{}` resolves outside superproject {}: {}",
                module.name,
                project.display(),
                child.display()
            );
        }

        let child_manifest = child.join(MANIFEST_FILE);
        if !submodule_manifest_present(&child_manifest).with_context(|| {
            format!(
                "validating {MANIFEST_FILE} for submodule `{}` at {}",
                module.name,
                child.display()
            )
        })? {
            eprintln!(
                "overtake: leaving non-Zed submodule `{}` at `{}` under Git authority (no {MANIFEST_FILE})",
                module.name, module.path
            );
            skipped.push((module.name.clone(), module.path.clone()));
            continue;
        }

        let manifest = read_manifest(&child).with_context(|| {
            format!(
                "submodule `{}` at {} contains an invalid {MANIFEST_FILE}",
                module.name,
                child.display()
            )
        })?;
        let package = manifest.full_name();
        if !seen_packages.insert(package.clone()) {
            bail!("multiple submodules declare the same Zed package `{package}`");
        }
        verify_checkout(&project, &module.path, &child)?;
        warn_on_repository_mismatch(module, &child, &manifest);
        imported.push((module.clone(), child, manifest));
    }

    if imported.is_empty() {
        let examples = skipped
            .iter()
            .take(8)
            .map(|(name, path)| format!("`{name}` at `{path}`"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "{} contains no overtake-compatible Zed submodules; {} configured submodule(s) have no {MANIFEST_FILE}: {examples}",
            project.display(),
            skipped.len()
        );
    }

    let manifest_path = project.join(MANIFEST_FILE);
    let previous_manifest = match fs::read(&manifest_path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", manifest_path.display()));
        }
    };
    let mut root = match previous_manifest.as_deref() {
        Some(contents) => {
            let text = std::str::from_utf8(contents)
                .with_context(|| format!("{} is not UTF-8", manifest_path.display()))?;
            Manifest::parse(text)
                .with_context(|| format!("invalid manifest {}", manifest_path.display()))?
        }
        None => generated_consumer_manifest(&project),
    };
    let root_package = root.full_name();
    let existing_members = collect_workspace_members(&project, &root)?;
    let mut member_paths: BTreeMap<String, String> = existing_members
        .into_iter()
        .map(|(package, member)| (package, member.path))
        .collect();

    let workspace = root.workspace.get_or_insert_with(WorkspaceSection::default);
    for (module, _child, manifest) in &imported {
        let package = manifest.full_name();
        if package == root_package {
            bail!(
                "submodule `{}` declares the same package identity as the root: `{package}`",
                module.name
            );
        }
        if let Some(existing) = member_paths.get(&package)
            && existing != &module.path
        {
            bail!(
                "workspace package `{package}` is already provided by `{existing}`, not submodule path `{}`",
                module.path
            );
        }
        member_paths.insert(package.clone(), module.path.clone());
        workspace.members.push(module.path.clone());

        let exact = exact_requirement(manifest.package.version_scheme, &manifest.package.version);
        if let Some(previous) = root.dependencies.insert(package.clone(), exact.clone())
            && previous != exact
        {
            eprintln!(
                "overtake: replaced `{package}` requirement `{previous}` with exact workspace requirement `{exact}`"
            );
        }
    }
    workspace.members.sort();
    workspace.members.dedup();
    root.validate()
        .context("validating overtaken root manifest")?;
    let manifest_text = set_manifest_consumes_gitmodules(&root.to_toml_string()?, true)?;

    ensure_manifest_unchanged(&manifest_path, previous_manifest.as_deref())?;
    let mut transaction = ProjectTransaction::begin(&project)?;
    transaction.backup(&manifest_path)?;
    fs::write(&manifest_path, manifest_text.as_bytes())
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    transaction.commit()?;

    // Run the ordinary installer after adoption. Because the imported packages
    // are workspace members, the complete solver never asks the registry for
    // them. The installer facade adds exact Git provenance to the resulting
    // lockfile after its normal transaction commits.
    if let Err(error) = crate::managed_install::install(
        &project,
        cfg,
        &[],
        false,
        InstallMode::Symlink,
        Adapter::Auto,
        false,
        None,
        false,
        false,
    ) {
        // The ordinary installer is transactional. Its only post-commit fallible
        // step is the additive Git-lock finalizer; in that case the adopted
        // manifest must remain aligned with the already committed install.
        if install_committed_before_error(&error) {
            return Err(error.context(
                "package installation committed; retained the overtaken manifest for reconciliation",
            ));
        }
        // ProjectTransaction::begin performs crash recovery. Serialize that
        // recovery with every live installer so this rollback cannot mistake a
        // different process's active staging journal for an abandoned one.
        let store = crate::store::Store::new(&cfg.home);
        let _rollback_lock = match store.install_lock() {
            Ok(lock) => lock,
            Err(rollback) => {
                return Err(error.context(format!(
                    "overtake installation failed and the root manifest could not be locked for rollback: {rollback:#}"
                )));
            }
        };
        if let Err(rollback) = restore_manifest_if_unchanged(
            &project,
            &manifest_path,
            manifest_text.as_bytes(),
            previous_manifest.as_deref(),
        ) {
            return Err(error.context(format!(
                "overtake installation failed and the root manifest could not be rolled back: {rollback:#}"
            )));
        }
        return Err(error.context("overtake installation failed; restored the prior root manifest"));
    }

    println!(
        "retained {} as a reversible Git transport mirror; Zed now owns dependency and lock authority",
        project.join(".gitmodules").display()
    );
    if !skipped.is_empty() {
        println!(
            "left {} non-Zed submodule(s) under Git authority",
            skipped.len()
        );
    }
    Ok(OvertakeReport {
        project,
        adopted: imported.len(),
    })
}

fn ensure_manifest_unchanged(path: &Path, expected: Option<&[u8]>) -> Result<()> {
    let current = match fs::read(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if current.as_deref() != expected {
        bail!(
            "{} changed while takeover was being planned; retry without overwriting another writer's content",
            path.display()
        );
    }
    Ok(())
}

fn install_committed_before_error(error: &anyhow::Error) -> bool {
    error.is::<crate::ops::GitLockFinalizeError>()
}

fn restore_manifest_if_unchanged(
    project: &Path,
    path: &Path,
    expected: &[u8],
    previous: Option<&[u8]>,
) -> Result<()> {
    let current = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if current != expected {
        bail!(
            "{} changed during takeover; refusing to overwrite another writer's content",
            path.display()
        );
    }

    let mut transaction = ProjectTransaction::begin(project)?;
    transaction.backup(path)?;
    if let Some(previous) = previous {
        fs::write(path, previous).with_context(|| format!("restoring {}", path.display()))?;
    }
    transaction.commit()
}

#[cfg(test)]
mod manifest_kind_tests {
    use std::fs;

    #[cfg(unix)]
    use super::preflight_gitmodules_metadata;
    use super::{submodule_manifest_present, validate_gitmodules_index};

    #[test]
    fn only_a_missing_manifest_is_skippable() {
        let project = tempfile::tempdir().unwrap();
        let manifest = project.path().join(".zpkg.toml");

        assert!(!submodule_manifest_present(&manifest).unwrap());
        fs::write(&manifest, "[package]\n").unwrap();
        assert!(submodule_manifest_present(&manifest).unwrap());
        fs::remove_file(&manifest).unwrap();
        fs::create_dir(&manifest).unwrap();

        let error = submodule_manifest_present(&manifest).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }

    #[test]
    fn gitmodules_index_requires_a_regular_stage_zero_blob() {
        validate_gitmodules_index(
            "100644 0123456789012345678901234567890123456789 0\t.gitmodules\n",
        )
        .unwrap();
        validate_gitmodules_index(
            "100755 0123456789012345678901234567890123456789 0\t.gitmodules\n",
        )
        .unwrap();

        for record in [
            "120000 0123456789012345678901234567890123456789 0\t.gitmodules\n",
            "100644 0123456789012345678901234567890123456789 1\t.gitmodules\n",
            "160000 0123456789012345678901234567890123456789 0\t.gitmodules\n",
        ] {
            let error = validate_gitmodules_index(record).unwrap_err();
            assert!(error.to_string().contains("regular Git blob"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_manifests_fail_closed() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let target = project.path().join("actual.toml");
        let manifest = project.path().join(".zpkg.toml");
        fs::write(&target, "[package]\n").unwrap();
        symlink(&target, &manifest).unwrap();

        let error = submodule_manifest_present(&manifest).unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_gitmodules_fail_before_git_parsing() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let target = project.path().join("external-gitmodules");
        fs::write(
            &target,
            "[submodule \"client\"]\n\tpath = vendor/client\n\turl = ../client\n",
        )
        .unwrap();
        symlink(&target, project.path().join(".gitmodules")).unwrap();

        let error = preflight_gitmodules_metadata(project.path()).unwrap_err();
        assert!(error.to_string().contains("must be a regular file"));
    }
}
