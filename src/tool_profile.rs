//! Frozen, offline replay of exact native tool artifacts into a project-local
//! executable profile.
//!
//! This is deliberately narrower than a backend resolver: it consumes only a
//! validated [`zed_interfaces::EnvironmentLock`], requires already-authenticated
//! archive bytes in Zed's local cache, reuses the existing content-addressed
//! store, and never executes installer scripts, hooks, plugins, package
//! managers, or downloaded binaries.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zed_interfaces::{
    EnvironmentLock, EnvironmentLockValidationMode, LockedArtifactFormat, LockedExecutable,
    LockedTool,
};

use crate::cli::InstallMode;
use crate::pack::sha256_file;
use crate::project_lock;
use crate::store::Store;

pub const TOOL_PROFILE_SCHEMA_V1: &str = "zed.tool-profile/v1";
const DEFAULT_LOCK_PATH: &str = ".zed/environment.lock.toml";
const DEFAULT_PROFILE_PATH: &str = ".zed/tools";
const PROFILE_STATE_FILE: &str = "profile.json";
const RAW_ARCHIVE_LAYOUT_EXTENSION: &str = "zed-pkg.archive-layout";
const RAW_ARCHIVE_LAYOUT: &str = "raw";
const MAX_RAW_TOOL_ENTRIES: usize = 200_000;
const MAX_RAW_TOOL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Local,
    Portable,
}

#[derive(Debug, Clone)]
pub struct LoadedEnvironmentLock {
    pub relative_path: PathBuf,
    pub lock: EnvironmentLock,
    pub digest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLockSummary {
    pub name: String,
    pub requirement: String,
    pub resolved: String,
    pub backend: String,
    pub target: String,
    pub artifact_sha256: String,
    pub artifact_size: u64,
    pub executables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolVerifyReceipt {
    pub schema: String,
    pub lock: String,
    pub lock_sha256: String,
    pub validation: String,
    pub plan_digest_sha256: String,
    pub tools: usize,
    pub variants: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInstallReceipt {
    pub schema: String,
    pub action: String,
    pub lock: String,
    pub lock_sha256: String,
    pub target: String,
    pub profile: String,
    pub bin: String,
    pub tools: Vec<ToolLockSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ToolProfileState {
    schema: String,
    lock_sha256: String,
    target: String,
    install_mode: String,
    tools: Vec<ToolProfileTool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ToolProfileTool {
    name: String,
    requirement: String,
    resolved: String,
    backend: String,
    artifact_sha256: String,
    install_root: String,
    executables: Vec<ToolProfileExecutable>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ToolProfileExecutable {
    name: String,
    source: String,
}

#[derive(Debug, Clone)]
struct SelectedTool<'a> {
    name: &'a str,
    locked: &'a LockedTool,
}

#[derive(Debug, Clone)]
struct PreparedExecutable {
    profile_name: String,
    source: PathBuf,
    source_relative: String,
}

#[derive(Debug, Clone)]
struct PreparedTool<'a> {
    selected: SelectedTool<'a>,
    install_root: PathBuf,
    executables: Vec<PreparedExecutable>,
}

pub fn default_lock_path() -> PathBuf {
    PathBuf::from(DEFAULT_LOCK_PATH)
}

pub fn default_profile_path() -> PathBuf {
    PathBuf::from(DEFAULT_PROFILE_PATH)
}

pub fn default_zed_home() -> Result<PathBuf> {
    let configured = env::var_os("ZED_PKG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".zed-pkg")))
        .context("could not determine Zed home; set ZED_PKG_HOME")?;
    if configured.is_absolute() {
        Ok(configured)
    } else {
        Ok(env::current_dir()?.join(configured))
    }
}

pub fn load_environment_lock(
    root: &Path,
    lock_path: Option<&Path>,
    mode: LockMode,
    expected_plan_digest: Option<&str>,
) -> Result<LoadedEnvironmentLock> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing project root `{}`", root.display()))?;
    ensure!(
        root.is_dir(),
        "project root `{}` is not a directory",
        root.display()
    );
    let relative_path = portable_relative_path(
        lock_path.unwrap_or_else(|| Path::new(DEFAULT_LOCK_PATH)),
        "environment lock",
        false,
    )?;
    let path = existing_regular_project_file(&root, &relative_path, "environment lock")?;
    let bytes = fs::read(&path)
        .with_context(|| format!("reading environment lock `{}`", relative_path.display()))?;
    let input = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "environment lock `{}` is not UTF-8",
            relative_path.display()
        )
    })?;
    let lock = if relative_path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        EnvironmentLock::parse_json(input)
    } else {
        EnvironmentLock::parse_toml(input)
    }
    .with_context(|| format!("parsing environment lock `{}`", relative_path.display()))?;
    lock.validate(match mode {
        LockMode::Local => EnvironmentLockValidationMode::Local,
        LockMode::Portable => EnvironmentLockValidationMode::Portable,
    })?;
    if let Some(expected) = expected_plan_digest {
        lock.verify_plan_digest(expected)?;
    }
    let digest_sha256 = lock.normalized_digest_sha256()?;
    Ok(LoadedEnvironmentLock {
        relative_path,
        lock,
        digest_sha256,
    })
}

pub fn verify_receipt(loaded: &LoadedEnvironmentLock, mode: LockMode) -> ToolVerifyReceipt {
    ToolVerifyReceipt {
        schema: TOOL_PROFILE_SCHEMA_V1.to_string(),
        lock: portable_display(&loaded.relative_path),
        lock_sha256: loaded.digest_sha256.clone(),
        validation: match mode {
            LockMode::Local => "local",
            LockMode::Portable => "portable",
        }
        .to_string(),
        plan_digest_sha256: loaded.lock.plan_digest_sha256.to_ascii_lowercase(),
        tools: loaded.lock.tools.len(),
        variants: loaded.lock.tools.values().map(Vec::len).sum(),
    }
}

pub fn list_target(loaded: &LoadedEnvironmentLock, target: &str) -> Result<Vec<ToolLockSummary>> {
    let selected = select_exact_target(&loaded.lock, target)?;
    Ok(selected
        .into_iter()
        .map(|selected| summary(selected.name, selected.locked))
        .collect())
}

pub fn install_offline(
    root: &Path,
    loaded: &LoadedEnvironmentLock,
    target: &str,
    profile_path: Option<&Path>,
    home: &Path,
) -> Result<ToolInstallReceipt> {
    install_offline_with_mode(
        root,
        loaded,
        target,
        profile_path,
        home,
        InstallMode::Symlink,
    )
}

/// Replay an exact cached tool lock using either store-backed links or a
/// project-owned copy. Copy mode is the OCI/export boundary: the complete
/// runtime roots and every command link remain below the profile directory.
pub fn install_offline_with_mode(
    root: &Path,
    loaded: &LoadedEnvironmentLock,
    target: &str,
    profile_path: Option<&Path>,
    home: &Path,
    mode: InstallMode,
) -> Result<ToolInstallReceipt> {
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing project root `{}`", root.display()))?;
    loaded
        .lock
        .validate(EnvironmentLockValidationMode::Portable)
        .context("frozen tool installation requires a portable environment lock")?;
    let profile_relative = portable_relative_path(
        profile_path.unwrap_or_else(|| Path::new(DEFAULT_PROFILE_PATH)),
        "tool profile",
        false,
    )?;
    validate_component(target, "target")?;
    let selected = select_exact_target(&loaded.lock, target)?;
    ensure!(
        !selected.is_empty(),
        "environment lock contains no tools for frozen profile installation"
    );
    let store = Store::new(home);

    project_lock::with_lock(&root, "install frozen environment tool profile", || {
        install_locked(
            &root,
            loaded,
            target,
            &profile_relative,
            &store,
            &selected,
            mode,
        )
    })
}

fn install_locked(
    root: &Path,
    loaded: &LoadedEnvironmentLock,
    target: &str,
    profile_relative: &Path,
    store: &Store,
    selected: &[SelectedTool<'_>],
    mode: InstallMode,
) -> Result<ToolInstallReceipt> {
    let profile_root = ensure_directory_chain(root, profile_relative, "tool profile")?;
    let version_root = ensure_real_directory(&profile_root, "v1", "tool profile version")?;
    let active = version_root.join(target);
    let prepared = prepare_tools(store, selected, mode)?;
    let state = profile_state(loaded, target, mode, &prepared);
    let mut state_bytes = serde_json::to_vec_pretty(&state)?;
    state_bytes.push(b'\n');
    let summaries = prepared
        .iter()
        .map(|prepared| summary(prepared.selected.name, prepared.selected.locked))
        .collect::<Vec<_>>();

    if active_profile_matches(&active, &state_bytes, mode, &prepared)? {
        if mode == InstallMode::Symlink {
            store.record_project(
                &active,
                prepared
                    .iter()
                    .map(|tool| tool.selected.locked.artifact.sha256.clone())
                    .collect(),
            )?;
        }
        return Ok(install_receipt(
            "unchanged",
            loaded,
            target,
            profile_relative,
            summaries,
        ));
    }

    let staging = tempfile::Builder::new()
        .prefix(&format!(".{target}.staging-"))
        .tempdir_in(&version_root)
        .context("creating tool-profile staging directory")?;
    let staging_path = staging.path().to_path_buf();
    let staging_bin = staging_path.join("bin");
    fs::create_dir(&staging_bin)?;
    let staging_roots = staging_path.join("roots");
    if mode == InstallMode::Copy {
        fs::create_dir(&staging_roots)?;
    }
    for tool in &prepared {
        if mode == InstallMode::Copy {
            validate_component(tool.selected.name, "tool name")?;
            copy_runtime_root(&tool.install_root, &staging_roots.join(tool.selected.name))?;
        }
        for executable in &tool.executables {
            let copy_target = PathBuf::from("..")
                .join("roots")
                .join(tool.selected.name)
                .join(&executable.source_relative);
            materialize_executable(
                &executable.source,
                &staging_bin.join(&executable.profile_name),
                (mode == InstallMode::Copy).then_some(copy_target.as_path()),
            )?;
        }
    }
    let state_path = staging_path.join(PROFILE_STATE_FILE);
    write_synced_file(&state_path, &state_bytes)?;
    sync_directory(&staging_bin)?;
    sync_directory(&staging_path)?;

    let staging_path = staging.keep();
    let backup = version_root.join(format!(".{target}.backup-{}", Uuid::new_v4()));
    let had_active = match fs::symlink_metadata(&active) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "active tool profile `{}` must be a real directory",
                portable_display(&active)
            );
            fs::rename(&active, &backup).with_context(|| {
                format!("staging prior tool profile `{}`", portable_display(&active))
            })?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspecting active tool profile"),
    };

    if let Err(error) = fs::rename(&staging_path, &active) {
        if had_active {
            let _ = fs::rename(&backup, &active);
        }
        let _ = fs::remove_dir_all(&staging_path);
        return Err(error).context("activating frozen tool profile");
    }
    sync_directory(&version_root)?;

    if mode == InstallMode::Symlink {
        let refs = prepared
            .iter()
            .map(|tool| tool.selected.locked.artifact.sha256.clone())
            .collect::<Vec<_>>();
        if let Err(error) = store.record_project(&active, refs) {
            let _ = fs::remove_dir_all(&active);
            if had_active {
                let _ = fs::rename(&backup, &active);
            }
            return Err(error).context("recording live tool-profile store references");
        }
    }
    if had_active {
        fs::remove_dir_all(&backup).context("removing prior tool profile")?;
    }
    sync_directory(&version_root)?;

    Ok(install_receipt(
        "installed",
        loaded,
        target,
        profile_relative,
        summaries,
    ))
}

fn prepare_tools<'a>(
    store: &Store,
    selected: &[SelectedTool<'a>],
    mode: InstallMode,
) -> Result<Vec<PreparedTool<'a>>> {
    let mut ownership = BTreeMap::<String, String>::new();
    let mut prepared = Vec::with_capacity(selected.len());
    for selected in selected {
        ensure!(
            matches!(
                selected.locked.artifact.format,
                LockedArtifactFormat::TarGz | LockedArtifactFormat::Zip
            ),
            "tool `{}` uses unsupported offline artifact format {:?}; this slice supports tar_gz and zip",
            selected.name,
            selected.locked.artifact.format
        );
        let archive = store.cached_artifact(&selected.locked.artifact.sha256);
        let metadata = fs::symlink_metadata(&archive).with_context(|| {
            format!(
                "missing cached artifact {} for tool `{}`; prefetch the exact lock before offline install",
                selected.locked.artifact.sha256, selected.name
            )
        })?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "cached artifact for tool `{}` must be a regular file",
            selected.name
        );
        ensure!(
            metadata.len() == selected.locked.artifact.size,
            "cached artifact size mismatch for tool `{}`: lock={}, cache={}",
            selected.name,
            selected.locked.artifact.size,
            metadata.len()
        );
        let (actual_sha256, actual_size) = sha256_file(&archive)?;
        ensure!(
            actual_sha256 == selected.locked.artifact.sha256,
            "cached artifact hash mismatch for tool `{}`: expected {}, got {}",
            selected.name,
            selected.locked.artifact.sha256,
            actual_sha256
        );
        ensure!(
            actual_size == selected.locked.artifact.size,
            "cached artifact size changed while verifying tool `{}`",
            selected.name
        );
        let raw_layout = selected
            .locked
            .artifact
            .extensions
            .get(RAW_ARCHIVE_LAYOUT_EXTENSION)
            .and_then(serde_json::Value::as_str)
            == Some(RAW_ARCHIVE_LAYOUT);
        ensure!(
            !raw_layout || mode == InstallMode::Copy,
            "tool `{}` uses an upstream runtime archive and requires --install-mode copy",
            selected.name
        );
        let package_root = if raw_layout {
            add_raw_tool_artifact(store, &archive, &selected.locked.artifact.sha256)?
        } else {
            store.add_artifact(&archive, &selected.locked.artifact.sha256)?
        };
        let install_root = resolve_directory_beneath(
            &package_root,
            Path::new(&selected.locked.install.root),
            &format!("tool `{}` install root", selected.name),
        )?;
        let mut executables = Vec::new();
        for executable in &selected.locked.install.executables {
            prepare_locked_executable(
                selected.name,
                executable,
                &install_root,
                &mut ownership,
                &mut executables,
            )?;
        }
        ensure!(
            !executables.is_empty(),
            "tool `{}` exposes no executables in the locked profile",
            selected.name
        );
        prepared.push(PreparedTool {
            selected: selected.clone(),
            install_root,
            executables,
        });
    }
    Ok(prepared)
}

fn add_raw_tool_artifact(store: &Store, archive: &Path, sha256: &str) -> Result<PathBuf> {
    let version_root = store.home().join("tool-store").join("v1");
    let entry = version_root.join(sha256);
    let active = entry.join("root");
    match fs::symlink_metadata(&active) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "raw tool-store entry `{}` must be a real directory",
                portable_display(&active)
            );
            return Ok(active);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspecting raw tool-store entry"),
    }

    let _store_lock = store.install_lock()?;
    if active.is_dir() {
        return Ok(active);
    }
    fs::create_dir_all(&version_root)?;
    let staging = tempfile::Builder::new()
        .prefix(&format!(".{sha256}.staging-"))
        .tempdir_in(&version_root)?;
    let staging_root = staging.path().join("root");
    fs::create_dir(&staging_root)?;
    extract_raw_tool_archive(archive, &staging_root)?;
    sync_directory(&staging_root)?;
    let staging_path = staging.keep();
    match fs::rename(&staging_path, &entry) {
        Ok(()) => {}
        Err(_) if active.is_dir() => {
            fs::remove_dir_all(&staging_path)?;
        }
        Err(error) => return Err(error).context("activating raw tool-store entry"),
    }
    sync_directory(&version_root)?;
    Ok(active)
}

fn extract_raw_tool_archive(archive: &Path, destination: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let decoder = GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    let mut entries = 0usize;
    let mut bytes = 0u64;
    for item in tar.entries()? {
        let mut item = item?;
        entries += 1;
        ensure!(
            entries <= MAX_RAW_TOOL_ENTRIES,
            "raw tool archive exceeds {MAX_RAW_TOOL_ENTRIES} entries"
        );
        let path = item.path()?.into_owned();
        let path = safe_archive_path(&path)?;
        let target = destination.join(&path);
        let kind = item.header().entry_type();
        match kind {
            tar::EntryType::Directory => {
                fs::create_dir_all(&target).with_context(|| {
                    format!(
                        "creating raw tool archive directory `{}`",
                        portable_display(&path)
                    )
                })?;
            }
            tar::EntryType::Regular => {
                let declared = item.header().size()?;
                bytes = bytes
                    .checked_add(declared)
                    .context("raw tool archive size overflow")?;
                ensure!(
                    bytes <= MAX_RAW_TOOL_BYTES,
                    "raw tool archive exceeds {} bytes unpacked",
                    MAX_RAW_TOOL_BYTES
                );
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "creating parent for raw tool archive file `{}`",
                            portable_display(&path)
                        )
                    })?;
                }
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                    .with_context(|| {
                        format!(
                            "creating raw tool archive file `{}`; the target filesystem may not preserve the archive's case-sensitive paths",
                            portable_display(&path)
                        )
                    })?;
                let mut limited = (&mut item).take(declared.saturating_add(1));
                let copied = std::io::copy(&mut limited, &mut output)?;
                ensure!(
                    copied == declared,
                    "raw tool archive entry `{}` size mismatch",
                    portable_display(&path)
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = item.header().mode()? & 0o777;
                    fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
                }
                output.sync_all()?;
            }
            tar::EntryType::Symlink => {
                let link = item
                    .link_name()?
                    .context("raw tool archive symlink has no target")?
                    .into_owned();
                validate_relative_symlink(&path, &link)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "creating parent for raw tool archive symlink `{}`",
                            portable_display(&path)
                        )
                    })?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&link, &target).with_context(|| {
                    format!(
                        "creating raw tool archive symlink `{}`; the target filesystem may not preserve the archive's case-sensitive paths",
                        portable_display(&path)
                    )
                })?;
                #[cfg(windows)]
                bail!(
                    "raw tool archive symlinks are not yet supported on Windows: `{}`",
                    portable_display(&path)
                );
            }
            other => bail!(
                "raw tool archive entry `{}` has unsupported type {other:?}",
                portable_display(&path)
            ),
        }
    }
    ensure!(entries > 0, "raw tool archive is empty");
    Ok(())
}

fn safe_archive_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        !path.as_os_str().is_empty(),
        "archive entry path cannot be empty"
    );
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => safe.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "raw tool archive entry `{}` escapes its extraction root",
                    portable_display(path)
                )
            }
        }
    }
    ensure!(
        !safe.as_os_str().is_empty(),
        "archive entry path cannot be empty"
    );
    Ok(safe)
}

fn validate_relative_symlink(entry: &Path, link: &Path) -> Result<()> {
    ensure!(
        !link.is_absolute(),
        "raw tool archive symlink `{}` must be relative",
        portable_display(entry)
    );
    let mut depth = entry.parent().map_or(0isize, |parent| {
        parent.components().count().try_into().unwrap_or(isize::MAX)
    });
    for component in link.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir => depth -= 1,
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "raw tool archive symlink `{}` has a non-portable target",
                    portable_display(entry)
                )
            }
        }
        ensure!(
            depth >= 0,
            "raw tool archive symlink `{}` escapes its extraction root",
            portable_display(entry)
        );
    }
    Ok(())
}

fn prepare_locked_executable(
    tool: &str,
    executable: &LockedExecutable,
    install_root: &Path,
    ownership: &mut BTreeMap<String, String>,
    output: &mut Vec<PreparedExecutable>,
) -> Result<()> {
    let source_relative = portable_relative_path(
        Path::new(&executable.path),
        &format!("tool `{tool}` executable `{}`", executable.name),
        false,
    )?;
    let source = resolve_regular_file_beneath(
        install_root,
        &source_relative,
        &format!("tool `{tool}` executable `{}`", executable.name),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            source.metadata()?.permissions().mode() & 0o111 != 0,
            "tool `{tool}` executable `{}` is not marked executable",
            executable.name
        );
    }
    for logical_name in std::iter::once(&executable.name).chain(executable.aliases.iter()) {
        validate_component(logical_name, "executable name")?;
        let profile_name = profile_executable_name(logical_name, &source)?;
        let key = profile_name.to_ascii_lowercase();
        if let Some(existing) = ownership.insert(key, tool.to_string()) {
            bail!(
                "executable `{profile_name}` is claimed by both tool `{existing}` and tool `{tool}`"
            );
        }
        output.push(PreparedExecutable {
            profile_name,
            source: source.clone(),
            source_relative: portable_display(&source_relative),
        });
    }
    output.sort_by(|left, right| left.profile_name.cmp(&right.profile_name));
    Ok(())
}

fn profile_state(
    loaded: &LoadedEnvironmentLock,
    target: &str,
    mode: InstallMode,
    prepared: &[PreparedTool<'_>],
) -> ToolProfileState {
    ToolProfileState {
        schema: TOOL_PROFILE_SCHEMA_V1.to_string(),
        lock_sha256: loaded.digest_sha256.clone(),
        target: target.to_string(),
        install_mode: match mode {
            InstallMode::Symlink => "symlink",
            InstallMode::Copy => "copy",
        }
        .to_string(),
        tools: prepared
            .iter()
            .map(|tool| ToolProfileTool {
                name: tool.selected.name.to_string(),
                requirement: tool.selected.locked.requirement.clone(),
                resolved: tool.selected.locked.resolved.clone(),
                backend: tool.selected.locked.backend.clone(),
                artifact_sha256: tool.selected.locked.artifact.sha256.clone(),
                install_root: tool.selected.locked.install.root.clone(),
                executables: tool
                    .executables
                    .iter()
                    .map(|executable| ToolProfileExecutable {
                        name: executable.profile_name.clone(),
                        source: executable.source_relative.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn active_profile_matches(
    active: &Path,
    expected_state: &[u8],
    mode: InstallMode,
    prepared: &[PreparedTool<'_>],
) -> Result<bool> {
    let metadata = match fs::symlink_metadata(active) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspecting active tool profile"),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "active tool profile `{}` must be a real directory",
        portable_display(active)
    );
    let state = active.join(PROFILE_STATE_FILE);
    let state_metadata = match fs::symlink_metadata(&state) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspecting tool profile state"),
    };
    if !state_metadata.is_file() || state_metadata.file_type().is_symlink() {
        return Ok(false);
    }
    if fs::read(&state)? != expected_state {
        return Ok(false);
    }
    let bin = active.join("bin");
    let bin_metadata = match fs::symlink_metadata(&bin) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("inspecting tool profile bin directory"),
    };
    if !bin_metadata.is_dir() || bin_metadata.file_type().is_symlink() {
        return Ok(false);
    }
    let expected = prepared
        .iter()
        .flat_map(|tool| {
            tool.executables
                .iter()
                .map(|executable| executable.profile_name.clone())
        })
        .collect::<BTreeSet<_>>();
    let actual = fs::read_dir(&bin)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<BTreeSet<_>>>()?;
    if expected != actual {
        return Ok(false);
    }
    for executable in prepared.iter().flat_map(|tool| tool.executables.iter()) {
        let destination = bin.join(&executable.profile_name);
        #[cfg(unix)]
        {
            let metadata = fs::symlink_metadata(&destination)?;
            let expected = if mode == InstallMode::Symlink {
                executable.source.clone()
            } else {
                let Some(tool) = prepared.iter().find(|tool| {
                    tool.executables.iter().any(|item| {
                        item.profile_name == executable.profile_name
                            && item.source_relative == executable.source_relative
                    })
                }) else {
                    return Ok(false);
                };
                PathBuf::from("..")
                    .join("roots")
                    .join(tool.selected.name)
                    .join(&executable.source_relative)
            };
            if !metadata.file_type().is_symlink() || fs::read_link(&destination)? != expected {
                return Ok(false);
            }
            if mode == InstallMode::Copy {
                let target = match fs::metadata(&destination) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error).context("inspecting copied tool command"),
                };
                if !target.is_file() {
                    return Ok(false);
                }
            }
        }
        #[cfg(windows)]
        {
            let metadata = fs::symlink_metadata(&destination)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Ok(false);
            }
            let (source_sha256, source_size) = sha256_file(&executable.source)?;
            let (destination_sha256, destination_size) = sha256_file(&destination)?;
            if source_sha256 != destination_sha256 || source_size != destination_size {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn install_receipt(
    action: &str,
    loaded: &LoadedEnvironmentLock,
    target: &str,
    profile_relative: &Path,
    tools: Vec<ToolLockSummary>,
) -> ToolInstallReceipt {
    let active_relative = profile_relative.join("v1").join(target);
    ToolInstallReceipt {
        schema: TOOL_PROFILE_SCHEMA_V1.to_string(),
        action: action.to_string(),
        lock: portable_display(&loaded.relative_path),
        lock_sha256: loaded.digest_sha256.clone(),
        target: target.to_string(),
        profile: portable_display(&active_relative),
        bin: portable_display(&active_relative.join("bin")),
        tools,
    }
}

fn summary(name: &str, locked: &LockedTool) -> ToolLockSummary {
    let mut executables = locked
        .install
        .executables
        .iter()
        .flat_map(|executable| {
            std::iter::once(executable.name.clone()).chain(executable.aliases.iter().cloned())
        })
        .collect::<Vec<_>>();
    executables.sort();
    ToolLockSummary {
        name: name.to_string(),
        requirement: locked.requirement.clone(),
        resolved: locked.resolved.clone(),
        backend: locked.backend.clone(),
        target: locked.platform.target.clone(),
        artifact_sha256: locked.artifact.sha256.clone(),
        artifact_size: locked.artifact.size,
        executables,
    }
}

fn select_exact_target<'a>(
    lock: &'a EnvironmentLock,
    target: &str,
) -> Result<Vec<SelectedTool<'a>>> {
    validate_component(target, "target")?;
    let mut selected = Vec::with_capacity(lock.tools.len());
    for (name, variants) in &lock.tools {
        let matching = variants
            .iter()
            .filter(|variant| variant.platform.target == target)
            .collect::<Vec<_>>();
        ensure!(
            !matching.is_empty(),
            "tool `{name}` has no locked variant for target `{target}`"
        );
        ensure!(
            matching.len() == 1,
            "tool `{name}` has {} locked variants for target `{target}`; multi-version activation is not yet certified",
            matching.len()
        );
        selected.push(SelectedTool {
            name,
            locked: matching[0],
        });
    }
    Ok(selected)
}

fn materialize_executable(
    source: &Path,
    destination: &Path,
    portable_target: Option<&Path>,
) -> Result<()> {
    ensure!(
        fs::symlink_metadata(destination)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "tool profile destination `{}` already exists",
        portable_display(destination)
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(portable_target.unwrap_or(source), destination).with_context(
            || {
                format!(
                    "linking tool executable `{}` -> `{}`",
                    portable_display(destination),
                    portable_display(source)
                )
            },
        )?;
    }
    #[cfg(windows)]
    {
        fs::copy(source, destination).with_context(|| {
            format!(
                "copying tool executable `{}` -> `{}`",
                portable_display(source),
                portable_display(destination)
            )
        })?;
        OpenOptions::new()
            .write(true)
            .open(destination)
            .with_context(|| {
                format!(
                    "opening copied tool executable `{}` for synchronization",
                    portable_display(destination)
                )
            })?
            .sync_all()
            .with_context(|| {
                format!(
                    "synchronizing copied tool executable `{}`",
                    portable_display(destination)
                )
            })?;
    }
    Ok(())
}

fn copy_runtime_root(source: &Path, destination: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(destination)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        "portable tool root destination `{}` already exists",
        portable_display(destination)
    );
    fs::create_dir(destination)?;
    for entry in walkdir::WalkDir::new(source)
        .follow_links(false)
        .min_depth(1)
    {
        let entry = entry.context("walking authenticated tool runtime root")?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .context("tool runtime entry escaped its root")?;
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            fs::create_dir(&target)?;
            fs::set_permissions(&target, metadata.permissions())?;
        } else if metadata.is_file() {
            fs::copy(entry.path(), &target)?;
            fs::set_permissions(&target, metadata.permissions())?;
        } else if metadata.file_type().is_symlink() {
            let link = fs::read_link(entry.path())?;
            ensure!(
                !link.is_absolute(),
                "tool runtime symlink `{}` must be relative",
                portable_display(relative)
            );
            let parent = relative.parent().unwrap_or_else(|| Path::new(""));
            let mut depth = parent.components().count() as isize;
            for component in link.components() {
                match component {
                    Component::Normal(_) => depth += 1,
                    Component::CurDir => {}
                    Component::ParentDir => depth -= 1,
                    Component::RootDir | Component::Prefix(_) => {
                        bail!(
                            "tool runtime symlink `{}` has a non-portable target",
                            portable_display(relative)
                        )
                    }
                }
                ensure!(
                    depth >= 0,
                    "tool runtime symlink `{}` escapes its authenticated root",
                    portable_display(relative)
                );
            }
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link, &target)?;
            #[cfg(windows)]
            bail!(
                "portable tool runtime symlinks are not yet supported on Windows: `{}`",
                portable_display(relative)
            );
        } else {
            bail!(
                "tool runtime entry `{}` is not a file, directory, or symlink",
                portable_display(relative)
            );
        }
    }
    Ok(())
}

fn profile_executable_name(logical_name: &str, source: &Path) -> Result<String> {
    #[cfg(not(windows))]
    let _ = source;
    #[cfg(windows)]
    {
        if Path::new(logical_name).extension().is_none()
            && let Some(extension) = source.extension().and_then(|value| value.to_str())
            && matches!(
                extension.to_ascii_lowercase().as_str(),
                "exe" | "com" | "cmd" | "bat"
            )
        {
            return Ok(format!("{logical_name}.{extension}"));
        }
    }
    Ok(logical_name.to_string())
}

fn resolve_directory_beneath(base: &Path, relative: &Path, field: &str) -> Result<PathBuf> {
    if relative == Path::new(".") {
        return base
            .canonicalize()
            .with_context(|| format!("canonicalizing {field}"));
    }
    let relative = portable_relative_path(relative, field, false)?;
    let candidate = walk_real_components(base, &relative, field)?;
    ensure!(candidate.is_dir(), "{field} must be a directory");
    Ok(candidate)
}

fn resolve_regular_file_beneath(base: &Path, relative: &Path, field: &str) -> Result<PathBuf> {
    let relative = portable_relative_path(relative, field, false)?;
    let candidate = walk_real_components(base, &relative, field)?;
    let metadata = fs::symlink_metadata(&candidate)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{field} must be a regular file"
    );
    Ok(candidate)
}

fn walk_real_components(base: &Path, relative: &Path, field: &str) -> Result<PathBuf> {
    let canonical_base = base
        .canonicalize()
        .with_context(|| format!("canonicalizing {field} base"))?;
    let mut current = canonical_base.clone();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("{field} must be a portable relative path");
        };
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .with_context(|| format!("inspecting {field} `{}`", portable_display(relative)))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "{field} must not traverse a symlink"
        );
    }
    let canonical = current
        .canonicalize()
        .with_context(|| format!("canonicalizing {field}"))?;
    ensure!(
        canonical.starts_with(&canonical_base),
        "{field} escapes its authenticated artifact root"
    );
    Ok(canonical)
}

fn existing_regular_project_file(root: &Path, relative: &Path, field: &str) -> Result<PathBuf> {
    let candidate = walk_real_components(root, relative, field)?;
    let metadata = fs::symlink_metadata(&candidate)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{field} `{}` must be a regular file",
        portable_display(relative)
    );
    Ok(candidate)
}

fn ensure_directory_chain(root: &Path, relative: &Path, field: &str) -> Result<PathBuf> {
    let relative = portable_relative_path(relative, field, false)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("{field} must be project-relative");
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "{field} component `{}` must be a real directory",
                portable_display(&current)
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!(
                        "creating {field} directory `{}`",
                        portable_display(&current)
                    )
                })?;
            }
            Err(error) => return Err(error).with_context(|| format!("inspecting {field}")),
        }
    }
    Ok(current)
}

fn ensure_real_directory(parent: &Path, name: &str, field: &str) -> Result<PathBuf> {
    validate_component(name, field)?;
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "{field} `{}` must be a real directory",
            portable_display(&path)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
        Err(error) => return Err(error).with_context(|| format!("inspecting {field}")),
    }
    Ok(path)
}

fn portable_relative_path(path: &Path, field: &str, allow_dot: bool) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "{field} path cannot be empty");
    ensure!(!path.is_absolute(), "{field} must be project-relative");
    let text = path.to_string_lossy();
    let windows_drive = text.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        && text.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    ensure!(
        !windows_drive
            && !text.starts_with('~')
            && !text.starts_with("$HOME")
            && !text.starts_with("${HOME}")
            && !text.starts_with("%USERPROFILE%")
            && !text.starts_with("//")
            && !text.starts_with("\\\\"),
        "{field} must be portable and project-relative"
    );
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => output.push(component),
            Component::CurDir if allow_dot => {}
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("{field} cannot escape the project root")
            }
        }
    }
    ensure!(
        allow_dot || !output.as_os_str().is_empty(),
        "{field} cannot be the project root"
    );
    Ok(output)
}

fn validate_component(value: &str, field: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{field} cannot be empty");
    ensure!(
        value == value.trim(),
        "{field} cannot contain surrounding whitespace"
    );
    ensure!(value != "." && value != "..", "{field} is invalid");
    ensure!(
        value
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '-')),
        "{field} `{value}` contains unsupported characters"
    );
    ensure!(value.len() <= 128, "{field} exceeds 128 bytes");
    Ok(())
}

fn write_synced_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating `{}`", portable_display(path)))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn portable_display(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_and_profile_paths_are_strict() {
        assert!(validate_component("x86_64-unknown-linux-gnu", "target").is_ok());
        assert!(validate_component("../linux", "target").is_err());
        assert!(portable_relative_path(Path::new(".zed/tools"), "profile", false).is_ok());
        assert!(portable_relative_path(Path::new("../tools"), "profile", false).is_err());
        assert!(portable_relative_path(Path::new(r"C:\\tools"), "profile", false).is_err());
    }

    #[test]
    fn exact_target_selection_rejects_missing_and_multiple_variants() {
        let lock = EnvironmentLock {
            plan_digest_sha256: "a".repeat(64),
            ..EnvironmentLock::default()
        };
        assert_eq!(select_exact_target(&lock, "x86_64-linux").unwrap().len(), 0);
    }

    #[test]
    fn profile_state_is_secret_free_by_shape() {
        let state = ToolProfileState {
            schema: TOOL_PROFILE_SCHEMA_V1.to_string(),
            lock_sha256: "a".repeat(64),
            target: "x86_64-linux".to_string(),
            install_mode: "copy".to_string(),
            tools: Vec::new(),
        };
        let json = serde_json::to_string(&state).unwrap();
        for forbidden in ["locator", "url", "token", "credential", "environment"] {
            assert!(!json.contains(forbidden));
        }
    }

    #[cfg(windows)]
    #[test]
    fn copied_windows_executable_is_opened_with_sync_capable_access() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("hello.cmd");
        let destination = root.path().join("hello-copy.cmd");
        fs::write(&source, b"@echo off\r\necho hello\r\n").unwrap();

        materialize_executable(&source, &destination).unwrap();

        assert_eq!(fs::read(destination).unwrap(), fs::read(source).unwrap());
    }
}
