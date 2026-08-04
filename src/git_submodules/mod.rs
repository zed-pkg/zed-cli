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

pub use cli::{OvertakeArgs, augment_root_command, dispatch};
pub(crate) use lock::{preflight_mutation, prepare_install, refresh_lock_extensions};

#[derive(Debug)]
pub struct OvertakeReport {
    pub project: PathBuf,
    pub adopted: usize,
}

/// Find the nearest Git-submodule superproject at or above `requested`.
pub fn find_root(requested: &Path) -> Option<PathBuf> {
    requested
        .ancestors()
        .find(|candidate| candidate.join(".gitmodules").is_file())
        .map(Path::to_path_buf)
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

/// Import every top-level `.gitmodules` entry that is itself a valid Zed
/// package. The root manifest becomes the graph authority while Git metadata is
/// retained as a reversible clone/update transport.
pub fn overtake(requested: &Path, cfg: &Config) -> Result<OvertakeReport> {
    let project = find_root(requested).with_context(|| {
        format!(
            "`zed overtake --git-submodules` requires .gitmodules at or above {}",
            requested.display()
        )
    })?;
    // Takeover is an authority migration, not merely a convenience checkout.
    // Refuse to fetch from working-tree-only or dirty transport metadata.
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
        let manifest = read_manifest(&child).with_context(|| {
            format!(
                "submodule `{}` at {} is not an overtake-compatible Zed package; add {MANIFEST_FILE} first",
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
    let manifest_text = root.to_toml_string()?;

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
