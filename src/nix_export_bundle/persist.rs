use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::RenderedNixExportBundle;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistNixExportBundleOutcome {
    Created,
    AlreadyCurrent,
}

pub fn persist_nix_export_bundle(
    rendered: &RenderedNixExportBundle,
    destination: &Path,
) -> Result<PersistNixExportBundleOutcome> {
    rendered
        .validate()
        .context("validating rendered Nix flake bundle before persistence")?;
    validate_destination(destination)?;

    let requested_parent = destination
        .parent()
        .context("Nix flake bundle output must have a parent directory")?;
    // Reject a caller-supplied leaf parent symlink, but resolve any existing
    // platform/administrator aliases above it (notably macOS `/var` ->
    // `/private/var`) once before allocating staging state. All subsequent
    // inspection, staging, and publication use this canonical directory.
    ensure_existing_directory(requested_parent)?;
    let parent = fs::canonicalize(requested_parent).with_context(|| {
        format!(
            "canonicalizing Nix flake bundle output parent `{}`",
            requested_parent.display()
        )
    })?;
    ensure_existing_directory(&parent)?;
    let file_name = destination
        .file_name()
        .context("Nix flake bundle output must name a directory")?;
    let destination = parent.join(file_name);

    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            verify_persisted_bundle(rendered, &destination)?;
            return Ok(PersistNixExportBundleOutcome::AlreadyCurrent);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "inspecting Nix flake bundle destination `{}`",
                    destination.display()
                )
            });
        }
    }

    let staging = fresh_staging_path(&destination)?;
    let mut cleanup = StagingCleanup::new(staging.clone());
    fs::create_dir(&staging).with_context(|| {
        format!(
            "creating Nix flake bundle staging directory `{}`",
            staging.display()
        )
    })?;

    write_rendered_bundle(rendered, &staging)?;
    verify_persisted_bundle(rendered, &staging)
        .context("verifying staged Nix flake bundle before atomic publication")?;
    sync_tree(&staging)?;

    match fs::rename(&staging, &destination) {
        Ok(()) => {
            cleanup.disarm();
            sync_directory(&parent)?;
            Ok(PersistNixExportBundleOutcome::Created)
        }
        Err(rename_error) => {
            if fs::symlink_metadata(&destination).is_ok() {
                verify_persisted_bundle(rendered, &destination).with_context(|| {
                    format!(
                        "another writer published a non-identical Nix flake bundle at `{}`",
                        destination.display()
                    )
                })?;
                Ok(PersistNixExportBundleOutcome::AlreadyCurrent)
            } else {
                Err(rename_error).with_context(|| {
                    format!(
                        "atomically publishing Nix flake bundle to `{}`",
                        destination.display()
                    )
                })
            }
        }
    }
}

pub fn verify_persisted_bundle(
    rendered: &RenderedNixExportBundle,
    destination: &Path,
) -> Result<()> {
    rendered.validate()?;
    let metadata = fs::symlink_metadata(destination).with_context(|| {
        format!(
            "inspecting persisted Nix flake bundle `{}`",
            destination.display()
        )
    })?;
    if metadata.file_type().is_symlink() {
        bail!(
            "persisted Nix flake bundle destination `{}` is a symbolic link",
            destination.display()
        );
    }
    if !metadata.is_dir() {
        bail!(
            "persisted Nix flake bundle destination `{}` is not a directory",
            destination.display()
        );
    }

    let persisted = collect_regular_files(destination)?;
    if persisted.len() != rendered.files.len() {
        bail!("persisted Nix flake bundle file set differs from rendered output");
    }

    for (relative, expected) in &rendered.files {
        let actual = persisted
            .get(relative)
            .with_context(|| format!("persisted Nix flake bundle is missing `{relative}`"))?;
        if actual != expected {
            bail!("persisted Nix flake bundle file `{relative}` differs from rendered bytes");
        }
        verify_regular_mode(&destination.join(relative), relative)?;
    }

    Ok(())
}

fn write_rendered_bundle(rendered: &RenderedNixExportBundle, staging: &Path) -> Result<()> {
    for (relative, bytes) in &rendered.files {
        validate_relative_path(relative)?;
        let destination = staging.join(relative);
        let parent = destination
            .parent()
            .context("rendered Nix flake bundle path has no parent")?;
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "creating staged Nix flake bundle directory `{}`",
                parent.display()
            )
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .with_context(|| {
                format!(
                    "creating staged Nix flake bundle file `{}`",
                    destination.display()
                )
            })?;
        file.write_all(bytes).with_context(|| {
            format!(
                "writing staged Nix flake bundle file `{}`",
                destination.display()
            )
        })?;
        set_regular_mode(&destination)?;
        file.sync_all().with_context(|| {
            format!(
                "synchronizing staged Nix flake bundle file `{}`",
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn collect_regular_files(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    collect_regular_files_at(root, root, &mut files)?;
    Ok(files)
}

fn collect_regular_files_at(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for entry in fs::read_dir(directory).with_context(|| {
        format!(
            "reading Nix flake bundle directory `{}`",
            directory.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!(
                "persisted Nix flake bundle contains symbolic link `{}`",
                path.display()
            );
        }
        if metadata.is_dir() {
            collect_regular_files_at(root, &path, files)?;
            continue;
        }
        if !metadata.is_file() {
            bail!(
                "persisted Nix flake bundle contains special file `{}`",
                path.display()
            );
        }
        let relative = path
            .strip_prefix(root)
            .context("persisted file escaped bundle root")?
            .to_string_lossy()
            .replace('\\', "/");
        validate_relative_path(&relative)?;
        let bytes = fs::read(&path)
            .with_context(|| format!("reading persisted Nix flake bundle file `{relative}`"))?;
        if files.insert(relative.clone(), bytes).is_some() {
            bail!("persisted Nix flake bundle contains duplicate path `{relative}`");
        }
    }
    Ok(())
}

fn validate_destination(destination: &Path) -> Result<()> {
    if destination.as_os_str().is_empty() {
        bail!("Nix flake bundle output path must not be empty");
    }
    for component in destination.components() {
        if matches!(component, Component::ParentDir) {
            bail!("Nix flake bundle output path must not contain `..`");
        }
    }
    match destination.file_name() {
        Some(name) if !name.is_empty() => Ok(()),
        _ => bail!("Nix flake bundle output path must name a directory"),
    }
}

fn validate_relative_path(relative: &str) -> Result<()> {
    if relative.is_empty() || relative.starts_with('/') || relative.contains('\\') {
        bail!("unsafe persisted Nix flake bundle path `{relative}`");
    }
    let path = Path::new(relative);
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("unsafe persisted Nix flake bundle path `{relative}`");
    }
    if relative.chars().any(|character| character.is_control()) {
        bail!("control-bearing persisted Nix flake bundle path `{relative}`");
    }
    Ok(())
}

fn ensure_existing_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting output parent `{}`", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "Nix flake bundle output parent `{}` is not a real directory",
            path.display()
        );
    }
    Ok(())
}

fn fresh_staging_path(destination: &Path) -> Result<PathBuf> {
    let parent = destination.parent().context("output has no parent")?;
    let name = destination
        .file_name()
        .context("output has no file name")?
        .to_string_lossy();
    for _ in 0..64 {
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = parent.join(format!(
            ".{name}.zed-nix-bundle-{}-{nanos}-{sequence}.tmp",
            std::process::id()
        ));
        if fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    bail!("could not allocate a fresh Nix flake bundle staging directory")
}

fn sync_tree(root: &Path) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            sync_tree(&path)?;
        }
    }
    sync_directory(root)
}

fn sync_directory(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(_path)
            .with_context(|| {
                format!(
                    "opening directory `{}` for synchronization",
                    _path.display()
                )
            })?
            .sync_all()
            .with_context(|| format!("synchronizing directory `{}`", _path.display()))?;
    }
    Ok(())
}

fn set_regular_mode(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

fn verify_regular_mode(_path: &Path, _relative: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::symlink_metadata(_path)?.permissions().mode() & 0o777;
        if mode != 0o644 {
            bail!("persisted Nix flake bundle file `{_relative}` has mode {mode:o}, expected 644");
        }
    }
    Ok(())
}

struct StagingCleanup {
    path: PathBuf,
    armed: bool,
}

impl StagingCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
