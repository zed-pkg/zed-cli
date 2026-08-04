use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use zed_interfaces::paths::ARCHIVE_ROOT;

use crate::nix_export_plan::NixExportPlan;

const MAX_ARCHIVE_ENTRIES: usize = 100_000;
const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ArchiveFile {
    mode: u32,
    size: u64,
}

pub(super) fn inspect_artifact(bytes: &[u8]) -> Result<BTreeMap<String, ArchiveFile>> {
    let decoder = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    let mut seen_paths = BTreeSet::new();
    let mut files = BTreeMap::new();
    let mut unpacked_bytes = 0_u64;
    let root_prefix = format!("{ARCHIVE_ROOT}/");

    for (index, entry) in archive
        .entries()
        .context("reading Zed artifact entries")?
        .enumerate()
    {
        if index >= MAX_ARCHIVE_ENTRIES {
            bail!("Zed artifact exceeds the {MAX_ARCHIVE_ENTRIES}-entry rendering limit");
        }
        let entry = entry.context("reading Zed artifact entry")?;
        let raw_path = entry.path_bytes();
        let raw_path = std::str::from_utf8(raw_path.as_ref())
            .context("Zed artifact contains a non-UTF-8 path")?;
        let path = normalize_archive_path(raw_path)?;
        if !seen_paths.insert(path.clone()) {
            bail!("Zed artifact contains duplicate path `{path}`");
        }

        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            if path != ARCHIVE_ROOT && !path.starts_with(&root_prefix) {
                bail!("Zed artifact directory `{path}` is outside canonical `{ARCHIVE_ROOT}/`");
            }
            continue;
        }
        if !entry_type.is_file() {
            bail!("Zed artifact path `{path}` is not a regular file or directory");
        }
        if !path.starts_with(&root_prefix) {
            bail!("Zed artifact file `{path}` is outside canonical `{ARCHIVE_ROOT}/`");
        }

        let size = entry.size();
        unpacked_bytes = unpacked_bytes
            .checked_add(size)
            .context("Zed artifact unpacked size overflow")?;
        if unpacked_bytes > MAX_UNPACKED_BYTES {
            bail!("Zed artifact exceeds the {MAX_UNPACKED_BYTES}-byte unpacked rendering limit");
        }
        let mode = entry
            .header()
            .mode()
            .with_context(|| format!("reading mode for Zed artifact path `{path}`"))?;
        if mode & !0o777 != 0 {
            bail!("Zed artifact path `{path}` has unsupported special mode bits");
        }
        files.insert(path, ArchiveFile { mode, size });
    }

    if files.is_empty() {
        bail!("Zed artifact contains no regular payload files");
    }
    Ok(files)
}

pub(super) fn verify_planned_bins(
    plan: &NixExportPlan,
    archive: &BTreeMap<String, ArchiveFile>,
) -> Result<()> {
    for (name, relative) in &plan.bins {
        let path = format!("{ARCHIVE_ROOT}/{relative}");
        let file = archive.get(&path).with_context(|| {
            format!("prebuilt bin `{name}` is absent from immutable artifact path `{path}`")
        })?;
        if file.mode & 0o111 == 0 {
            bail!("prebuilt bin `{name}` is not executable in immutable artifact path `{path}`");
        }
        if file.size == 0 {
            bail!("prebuilt bin `{name}` is empty in immutable artifact path `{path}`");
        }
    }
    Ok(())
}

fn normalize_archive_path(raw: &str) -> Result<String> {
    let path = raw.strip_suffix('/').unwrap_or(raw);
    if path.is_empty() || path.starts_with('/') || path.contains('\\') || path.contains('\0') {
        bail!("unsafe Zed artifact path `{raw}`");
    }
    for component in path.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            bail!("unsafe Zed artifact path `{raw}`");
        }
    }
    Ok(path.to_string())
}
