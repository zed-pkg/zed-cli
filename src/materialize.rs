//! Project-local package materialization.
//!
//! Registry artifacts are extracted into an immutable, content-addressed store.
//! Symlink mode points at a canonical store/workspace directory. Copy mode
//! stages a self-contained tree and never emits symlinks, which keeps OCI
//! layers and copied workspaces independent from the global store.
//!
//! Workspace source trees are less trusted than registry artifacts: they may
//! contain symlinks or special files that the archive extractor would reject.
//! This module therefore applies the same boundary checks while copying.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::cli::InstallMode;

/// Match the archive extractor's inode-exhaustion ceiling.
const MAX_COPY_ENTRIES: usize = 200_000;
/// Bound recursive traversal for adversarial workspace trees and symlink
/// expansion. Normal package source trees are many orders of magnitude
/// shallower than this.
const MAX_COPY_DEPTH: usize = 256;

#[derive(Default)]
struct CopyBudget {
    entries: usize,
}

impl CopyBudget {
    fn charge(&mut self, source: &Path) -> Result<()> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > MAX_COPY_ENTRIES {
            bail!(
                "package copy exceeds the {MAX_COPY_ENTRIES}-entry limit while visiting {}; refusing",
                source.display()
            );
        }
        Ok(())
    }
}

fn canonical_source_directory(source: &Path) -> Result<PathBuf> {
    let canonical = source.canonicalize().with_context(|| {
        format!(
            "resolving package materialization source {}",
            source.display()
        )
    })?;
    if !canonical.is_dir() {
        bail!(
            "package materialization source {} is not a directory",
            canonical.display()
        );
    }
    Ok(canonical)
}

/// Resolve a destination without creating any missing parent directories.
///
/// This lets the overlap check run before the materializer mutates either tree.
/// The nearest existing ancestor is canonicalized so symlinked parents cannot
/// disguise a destination inside the source tree.
fn canonical_destination(destination: &Path) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .context("package materialization destination has no file name")?;
    let parent = destination
        .parent()
        .context("package materialization destination has no parent")?;

    let mut existing = parent;
    let mut missing = Vec::<OsString>::new();
    while fs::symlink_metadata(existing).is_err() {
        let component = existing.file_name().with_context(|| {
            format!(
                "cannot locate an existing ancestor for materialization destination {}",
                destination.display()
            )
        })?;
        missing.push(component.to_os_string());
        existing = existing.parent().with_context(|| {
            format!(
                "cannot locate an existing ancestor for materialization destination {}",
                destination.display()
            )
        })?;
    }

    let mut canonical_parent = existing.canonicalize().with_context(|| {
        format!(
            "resolving materialization ancestor {} for {}",
            existing.display(),
            destination.display()
        )
    })?;
    for component in missing.iter().rev() {
        canonical_parent.push(component);
    }
    Ok(canonical_parent.join(name))
}

fn reject_overlap(source: &Path, destination: &Path) -> Result<()> {
    if source == destination || destination.starts_with(source) || source.starts_with(destination) {
        bail!(
            "package materialization source {} overlaps destination {}; refusing to remove, recurse into, or link a tree through itself",
            source.display(),
            destination.display()
        );
    }
    Ok(())
}

fn canonical_materialization_paths(
    source: &Path,
    destination: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let source = canonical_source_directory(source)?;
    let destination = canonical_destination(destination)?;
    reject_overlap(&source, &destination)?;
    Ok((source, destination))
}

pub(crate) fn replace_destination(destination: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(destination) {
        if metadata.file_type().is_dir() {
            fs::remove_dir_all(destination).with_context(|| {
                format!(
                    "removing previous materialized directory {}",
                    destination.display()
                )
            })?;
        } else {
            fs::remove_file(destination).with_context(|| {
                format!(
                    "removing previous materialized file or link {}",
                    destination.display()
                )
            })?;
        }
    }
    Ok(())
}

fn ensure_inside_root(root: &Path, resolved: &Path, source: &Path) -> Result<()> {
    if !resolved.starts_with(root) {
        bail!(
            "workspace package entry {} resolves outside package root {} to {}; refusing copy-mode escape",
            source.display(),
            root.display(),
            resolved.display()
        );
    }
    Ok(())
}

fn copy_node(
    root: &Path,
    source: &Path,
    destination: &Path,
    active_directories: &mut BTreeSet<PathBuf>,
    budget: &mut CopyBudget,
    depth: usize,
) -> Result<()> {
    if depth > MAX_COPY_DEPTH {
        bail!(
            "package copy exceeds the maximum directory/symlink depth of {MAX_COPY_DEPTH} at {}; refusing",
            source.display()
        );
    }
    budget.charge(source)?;

    let lexical_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("reading package entry metadata {}", source.display()))?;
    let resolved = if lexical_metadata.file_type().is_symlink() {
        let resolved = source
            .canonicalize()
            .with_context(|| format!("resolving package symlink {}", source.display()))?;
        ensure_inside_root(root, &resolved, source)?;
        resolved
    } else {
        source.to_path_buf()
    };
    let metadata = fs::metadata(&resolved)
        .with_context(|| format!("reading resolved package entry {}", resolved.display()))?;

    if metadata.is_dir() {
        let canonical = resolved.canonicalize().with_context(|| {
            format!(
                "resolving package directory while copying {}",
                resolved.display()
            )
        })?;
        ensure_inside_root(root, &canonical, source)?;
        if !active_directories.insert(canonical.clone()) {
            bail!(
                "package copy encountered a directory/symlink cycle at {} -> {}; refusing",
                source.display(),
                canonical.display()
            );
        }

        fs::create_dir_all(destination).with_context(|| {
            format!(
                "creating staged package directory {}",
                destination.display()
            )
        })?;
        let mut entries = fs::read_dir(&canonical)
            .with_context(|| format!("reading package directory {}", canonical.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            copy_node(
                root,
                &entry.path(),
                &destination.join(entry.file_name()),
                active_directories,
                budget,
                depth.saturating_add(1),
            )?;
        }
        active_directories.remove(&canonical);
        return Ok(());
    }

    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&resolved, destination).with_context(|| {
            format!(
                "copying package file {} to {}",
                resolved.display(),
                destination.display()
            )
        })?;
        return Ok(());
    }

    bail!(
        "package entry {} resolves to unsupported filesystem object {}; only files, directories, and in-package symlinks are allowed",
        source.display(),
        resolved.display()
    )
}

fn copy_directory_canonical(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("copy destination has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating package copy parent {}", parent.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".zed-materialize-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "creating package copy staging directory in {}",
                parent.display()
            )
        })?;
    let staged_tree = staging.path().join("package");
    let mut active_directories = BTreeSet::new();
    let mut budget = CopyBudget::default();
    copy_node(
        source,
        source,
        &staged_tree,
        &mut active_directories,
        &mut budget,
        0,
    )?;

    // No consumer-visible output is removed until the complete source tree has
    // passed validation and copied successfully.
    replace_destination(destination)?;
    fs::rename(&staged_tree, destination).with_context(|| {
        format!(
            "publishing staged package copy to {}",
            destination.display()
        )
    })?;
    Ok(())
}

/// Copy a directory through the same canonicalization and overlap checks used
/// by dependency materialization. Lifecycle staging and build-output copying
/// call this directly, so it must not rely on callers to pre-validate paths.
pub(crate) fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    let (source, destination) = canonical_materialization_paths(source, destination)?;
    copy_directory_canonical(&source, &destination)
}

/// Materialize one package directory.
///
/// * Symlink mode always writes an absolute canonical target, avoiding links
///   whose meaning changes with destination depth or invocation directory.
/// * Copy mode stages a symlink-free tree, rejects external links/cycles and
///   only replaces an existing destination after the full copy succeeds.
/// * Both modes reject source/destination overlap before deleting anything.
pub(crate) fn link_or_copy(source: &Path, destination: &Path, mode: InstallMode) -> Result<()> {
    let (source, destination) = canonical_materialization_paths(source, destination)?;

    match mode {
        InstallMode::Symlink => {
            let parent = destination
                .parent()
                .context("symlink destination has no parent")?;
            fs::create_dir_all(parent)
                .with_context(|| format!("creating package symlink parent {}", parent.display()))?;
            replace_destination(&destination)?;
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&source, &destination).with_context(|| {
                    format!(
                        "linking materialized package {} -> {}",
                        destination.display(),
                        source.display()
                    )
                })?;
                Ok(())
            }
            #[cfg(not(unix))]
            {
                let _ = source;
                let _ = destination;
                bail!("symlink install mode was not normalized before materialization")
            }
        }
        InstallMode::Copy => copy_directory_canonical(&source, &destination),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_mode_uses_an_absolute_canonical_target() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("consumer/deps/acme/pkg");
        write(&source.join("payload.txt"), "payload\n");

        link_or_copy(&source, &destination, InstallMode::Symlink).unwrap();

        let target = fs::read_link(&destination).unwrap();
        assert!(
            target.is_absolute(),
            "symlink target was relative: {target:?}"
        );
        assert_eq!(target, source.canonicalize().unwrap());
        assert_eq!(
            fs::read_to_string(destination.join("payload.txt")).unwrap(),
            "payload\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_mode_dereferences_only_in_package_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("consumer/pkg");
        write(&source.join("real/file.txt"), "inside\n");
        std::os::unix::fs::symlink("real", source.join("alias")).unwrap();

        link_or_copy(&source, &destination, InstallMode::Copy).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("alias/file.txt")).unwrap(),
            "inside\n"
        );
        assert!(
            !fs::symlink_metadata(destination.join("alias"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_mode_rejects_external_symlinks_before_replacing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let outside = temp.path().join("outside/secret.txt");
        let destination = temp.path().join("consumer/pkg");
        write(&outside, "secret\n");
        write(&source.join("safe.txt"), "safe\n");
        fs::create_dir_all(&destination).unwrap();
        write(&destination.join("existing.txt"), "keep\n");
        std::os::unix::fs::symlink(&outside, source.join("escape")).unwrap();

        let error = link_or_copy(&source, &destination, InstallMode::Copy)
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside package root"), "{error}");
        assert_eq!(
            fs::read_to_string(destination.join("existing.txt")).unwrap(),
            "keep\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_mode_rejects_symlink_cycles_before_replacing_destination() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("consumer/pkg");
        write(&source.join("dir/file.txt"), "payload\n");
        std::os::unix::fs::symlink(&source, source.join("dir/back")).unwrap();
        write(&destination.join("existing.txt"), "keep\n");

        let error = link_or_copy(&source, &destination, InstallMode::Copy)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cycle"), "{error}");
        assert_eq!(
            fs::read_to_string(destination.join("existing.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn overlapping_source_and_destination_are_rejected_without_source_loss() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        write(&source.join("payload.txt"), "payload\n");

        let destination = source.join("nested/output");
        let error = copy_directory(&source, &destination)
            .unwrap_err()
            .to_string();
        assert!(error.contains("overlaps destination"), "{error}");
        assert_eq!(
            fs::read_to_string(source.join("payload.txt")).unwrap(),
            "payload\n"
        );
        assert!(
            !source.join("nested").exists(),
            "overlap rejection created directories inside the source tree"
        );
    }
}
