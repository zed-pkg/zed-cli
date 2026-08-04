use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};

use super::git::{
    SubmoduleConfig, WorkspaceMember, checked_git, collect_workspace_members,
    configured_submodules, is_git_object_id, origin_url, validate_relative_path,
    verify_checkout, verify_gitmodules_committed,
};
use crate::config::read_manifest;
use crate::pack;
use crate::transaction::ProjectTransaction;

const LOCK_EXTENSION_KEY: &str = "git-submodule";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitSubmoduleLock {
    pub name: String,
    pub path: String,
    pub package: String,
    pub version: String,
    pub url: String,
    pub commit: String,
    pub sha256: String,
    pub size: u64,
    pub format: ArtifactFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LockExtensions {
    #[serde(default, rename = "git-submodule")]
    git_submodules: Vec<GitSubmoduleLock>,
}

#[derive(Debug, Serialize)]
struct LockExtensionDocument<'a> {
    #[serde(rename = "git-submodule")]
    git_submodules: &'a [GitSubmoduleLock],
}

#[derive(Debug)]
pub(crate) enum InstallLockPlan {
    Frozen,
    Refresh(Vec<GitSubmoduleLock>),
}

impl InstallLockPlan {
    pub(crate) fn finish(self, project: &Path) -> Result<()> {
        match self {
            InstallLockPlan::Frozen => Ok(()),
            InstallLockPlan::Refresh(entries) => write_lock_extensions(project, &entries),
        }
    }
}

/// Verify or precompute additive Git lock records before the ordinary installer
/// mutates anything. Frozen mode compares every field and never rewrites bytes.
pub(crate) fn prepare_install(project: &Path, frozen: bool) -> Result<InstallLockPlan> {
    if !project.join(MANIFEST_FILE).is_file() {
        let previous = read_lock_extensions(project)?;
        if frozen && !previous.is_empty() {
            bail!(
                "--frozen Git-submodule replay requires {MANIFEST_FILE}; the lock extension cannot map packages to workspace paths without it"
            );
        }
        return Ok(if frozen {
            InstallLockPlan::Frozen
        } else {
            InstallLockPlan::Refresh(Vec::new())
        });
    }

    let manifest = read_manifest(project)?;
    let previous = read_lock_extensions(project)?;
    let current = current_lock_entries(project, &manifest, &previous)?;
    if frozen {
        compare_frozen_entries(&previous, &current)?;
        Ok(InstallLockPlan::Frozen)
    } else {
        Ok(InstallLockPlan::Refresh(current))
    }
}

/// Preflight an add/remove operation before its internal installer can replace
/// the lockfile. The resulting plan is intentionally discarded; the manifest
/// may change, so [`refresh_lock_extensions`] recomputes active workspace
/// reachability after the operation succeeds.
pub(crate) fn preflight_mutation(project: &Path) -> Result<()> {
    let _ = prepare_install(project, false)?;
    Ok(())
}

pub(crate) fn refresh_lock_extensions(project: &Path) -> Result<()> {
    if !project.join(MANIFEST_FILE).is_file() || !project.join(LOCKFILE_FILE).is_file() {
        return Ok(());
    }
    let manifest = read_manifest(project)?;
    let previous = read_lock_extensions(project)?;
    let current = current_lock_entries(project, &manifest, &previous)?;
    write_lock_extensions(project, &current)
}

fn current_lock_entries(
    project: &Path,
    manifest: &Manifest,
    previous: &[GitSubmoduleLock],
) -> Result<Vec<GitSubmoduleLock>> {
    let members = collect_workspace_members(project, manifest)?;
    let active = active_workspace_packages(manifest, &members);
    let configured: BTreeMap<String, SubmoduleConfig> = configured_submodules(project)?
        .into_iter()
        .map(|module| (module.path.clone(), module))
        .collect();
    let mut verified_gitmodules = false;
    let previous_by_package: BTreeMap<String, &GitSubmoduleLock> = previous
        .iter()
        .map(|entry| (entry.package.clone(), entry))
        .collect();

    let mut entries = Vec::new();
    for package in active {
        let member = members
            .get(&package)
            .expect("active workspace package must have a discovered member");
        let module = configured.get(&member.path);
        let prior = previous_by_package.get(&package).copied();
        if module.is_none() && prior.is_none() {
            continue;
        }
        if module.is_some() && !verified_gitmodules {
            verify_gitmodules_committed(project)?;
            verified_gitmodules = true;
        }
        if let Some(prior) = prior
            && prior.path != member.path
        {
            bail!(
                "Git lock entry `{package}` points at `{}`, but the active workspace provides it from `{}`",
                prior.path,
                member.path
            );
        }
        entries.push(build_lock_entry(project, member, module, prior)?);
    }
    entries.sort_by(|left, right| {
        (&left.package, &left.path, &left.name).cmp(&(&right.package, &right.path, &right.name))
    });
    validate_lock_entries(&entries)?;
    Ok(entries)
}

fn compare_frozen_entries(
    locked: &[GitSubmoduleLock],
    current: &[GitSubmoduleLock],
) -> Result<()> {
    validate_lock_entries(locked)?;
    if locked == current {
        return Ok(());
    }

    let locked_by_package: BTreeMap<&str, &GitSubmoduleLock> = locked
        .iter()
        .map(|entry| (entry.package.as_str(), entry))
        .collect();
    let current_by_package: BTreeMap<&str, &GitSubmoduleLock> = current
        .iter()
        .map(|entry| (entry.package.as_str(), entry))
        .collect();
    let packages: BTreeSet<&str> = locked_by_package
        .keys()
        .chain(current_by_package.keys())
        .copied()
        .collect();
    for package in packages {
        match (
            locked_by_package.get(package),
            current_by_package.get(package),
        ) {
            (None, Some(_)) => bail!(
                "--frozen: adopted Git submodule `{package}` is missing from {LOCKFILE_FILE}"
            ),
            (Some(_), None) => bail!(
                "--frozen: {LOCKFILE_FILE} contains stale Git submodule `{package}` that is not active in the workspace graph"
            ),
            (Some(expected), Some(actual)) if *expected != *actual => {
                let fields = changed_lock_fields(expected, actual);
                bail!(
                    "--frozen: Git submodule `{package}` drifted in {}; run a non-frozen install after reviewing the change",
                    fields.join(", ")
                );
            }
            _ => {}
        }
    }
    bail!("--frozen: Git submodule lock metadata drifted")
}

fn changed_lock_fields(expected: &GitSubmoduleLock, actual: &GitSubmoduleLock) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if expected.name != actual.name {
        fields.push("name");
    }
    if expected.path != actual.path {
        fields.push("path");
    }
    if expected.version != actual.version {
        fields.push("version");
    }
    if expected.url != actual.url {
        fields.push("url");
    }
    if expected.commit != actual.commit {
        fields.push("commit");
    }
    if expected.sha256 != actual.sha256 {
        fields.push("sha256");
    }
    if expected.size != actual.size {
        fields.push("size");
    }
    if expected.format != actual.format {
        fields.push("format");
    }
    if expected.branch != actual.branch {
        fields.push("branch");
    }
    fields
}

fn build_lock_entry(
    project: &Path,
    member: &WorkspaceMember,
    module: Option<&SubmoduleConfig>,
    previous: Option<&GitSubmoduleLock>,
) -> Result<GitSubmoduleLock> {
    let commit = verify_checkout(project, &member.path, &member.root)?;
    // The committed `.gitmodules` declaration is the reproducible transport
    // authority. A checkout's local `origin` can legitimately be rewritten by
    // mirrors or developer tooling, so use it only when Git metadata has been
    // intentionally removed after takeover and the lock has no prior URL.
    let remote = module
        .map(|module| module.url.clone())
        .or_else(|| previous.map(|entry| entry.url.clone()))
        .or_else(|| origin_url(&member.root))
        .context("adopted Git submodule has no transport URL")?;
    let name = module
        .map(|module| module.name.clone())
        .or_else(|| previous.map(|entry| entry.name.clone()))
        .unwrap_or_else(|| member.path.clone());
    let branch = module
        .and_then(|module| module.branch.clone())
        .or_else(|| previous.and_then(|entry| entry.branch.clone()));

    let packed = pack_commit(&member.root, &member.manifest)?;
    Ok(GitSubmoduleLock {
        name,
        path: member.path.clone(),
        package: member.manifest.full_name(),
        version: member.manifest.package.version.clone(),
        url: remote,
        commit,
        sha256: packed.sha256,
        size: packed.size,
        format: packed.format,
        branch,
    })
}

fn pack_commit(project: &Path, manifest: &Manifest) -> Result<pack::PackResult> {
    let workspace = tempfile::tempdir().context("creating Git archive verification workspace")?;
    let archive_path = workspace.path().join("source.tar");
    let tree = workspace.path().join("tree");
    let output = workspace.path().join("out");
    fs::create_dir_all(&tree)?;

    let archive_arg = archive_path
        .to_str()
        .context("temporary Git archive path is not UTF-8")?;
    checked_git(
        project,
        &["archive", "--format=tar", "--output", archive_arg, "HEAD"],
    )
    .context("archiving adopted Git submodule commit")?;
    let file = fs::File::open(&archive_path)?;
    let mut archive = tar::Archive::new(file);
    archive
        .unpack(&tree)
        .context("extracting adopted Git submodule commit")?;

    let mut lock_manifest = manifest.clone();
    // A checked-out submodule uses a `.git` pointer file. The normal package
    // defaults exclude `.git/**`; add the exact file spelling as defense in
    // depth even though `git archive` does not emit it.
    lock_manifest.publish.exclude.push(".git".to_string());
    pack::pack(&tree, &lock_manifest, Some(&output))
        .context("packing canonical adopted Git submodule artifact")
}

pub(super) fn active_workspace_packages(
    root: &Manifest,
    members: &BTreeMap<String, WorkspaceMember>,
) -> BTreeSet<String> {
    let mut active = BTreeSet::new();
    let mut queue: VecDeque<String> = root
        .dependencies
        .keys()
        .chain(root.build_dependencies.keys())
        .cloned()
        .collect();
    while let Some(package) = queue.pop_front() {
        let Some(member) = members.get(&package) else {
            continue;
        };
        if !active.insert(package) {
            continue;
        }
        queue.extend(
            member
                .manifest
                .dependencies
                .keys()
                .chain(member.manifest.build_dependencies.keys())
                .cloned(),
        );
    }
    active
}

pub(super) fn read_lock_extensions(project: &Path) -> Result<Vec<GitSubmoduleLock>> {
    let path = project.join(LOCKFILE_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut entries = toml::from_str::<LockExtensions>(&text)
        .with_context(|| format!("parsing Git extensions in {}", path.display()))?
        .git_submodules;
    entries.sort_by(|left, right| {
        (&left.package, &left.path, &left.name).cmp(&(&right.package, &right.path, &right.name))
    });
    validate_lock_entries(&entries)?;
    Ok(entries)
}

pub(super) fn write_lock_extensions(project: &Path, entries: &[GitSubmoduleLock]) -> Result<()> {
    let path = project.join(LOCKFILE_FILE);
    if !path.is_file() {
        if entries.is_empty() {
            return Ok(());
        }
        bail!("cannot write Git submodule metadata without {LOCKFILE_FILE}");
    }
    validate_lock_entries(entries)?;

    let text = fs::read_to_string(&path)?;
    // Prove the additive extension remains readable by the canonical lock
    // parser before and after mutation.
    zed_interfaces::lockfile::Lockfile::parse(&text)
        .with_context(|| format!("invalid canonical lock data in {}", path.display()))?;
    let mut table: toml::Table = toml::from_str(&text)?;
    let had_extension = table.remove(LOCK_EXTENSION_KEY).is_some();
    if entries.is_empty() && !had_extension {
        return Ok(());
    }
    if !entries.is_empty() {
        let extension = toml::to_string(&LockExtensionDocument {
            git_submodules: entries,
        })?;
        let mut extension: toml::Table = toml::from_str(&extension)?;
        let value = extension
            .remove(LOCK_EXTENSION_KEY)
            .context("serialized Git lock extension disappeared")?;
        table.insert(LOCK_EXTENSION_KEY.to_string(), value);
    }
    let encoded = toml::to_string_pretty(&table)?;
    zed_interfaces::lockfile::Lockfile::parse(&encoded)
        .context("Git extension made the canonical lock unreadable")?;

    let mut transaction = ProjectTransaction::begin(project)?;
    transaction.backup(&path)?;
    fs::write(&path, encoded).with_context(|| format!("writing {}", path.display()))?;
    transaction.commit()?;
    if !entries.is_empty() {
        println!(
            "recorded {} adopted Git submodule(s) in {}",
            entries.len(),
            path.display()
        );
    }
    Ok(())
}

fn validate_lock_entries(entries: &[GitSubmoduleLock]) -> Result<()> {
    let mut packages = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for entry in entries {
        if entry.name.trim().is_empty() {
            bail!("Git submodule lock entry has an empty name");
        }
        validate_relative_path(&entry.path)?;
        let (org, name) = crate::ops::split_key(&entry.package)?;
        if format!("{org}/{name}") != entry.package {
            bail!("non-canonical Git submodule package `{}`", entry.package);
        }
        if entry.version.trim().is_empty() {
            bail!("Git submodule `{}` has an empty version", entry.package);
        }
        if entry.url.trim().is_empty() {
            bail!("Git submodule `{}` has an empty URL", entry.package);
        }
        if !is_git_object_id(&entry.commit) {
            bail!(
                "Git submodule `{}` has invalid immutable commit `{}`",
                entry.package,
                entry.commit
            );
        }
        crate::store::require_sha256(&entry.sha256)?;
        if entry.size == 0 {
            bail!("Git submodule `{}` has a zero-byte artifact", entry.package);
        }
        if !packages.insert(entry.package.clone()) {
            bail!("duplicate Git submodule package `{}` in lock", entry.package);
        }
        if !paths.insert(entry.path.clone()) {
            bail!("duplicate Git submodule path `{}` in lock", entry.path);
        }
    }
    Ok(())
}
