//! Hardened normalization of audited native-registry artifacts into Zed's
//! canonical `pkg/` store shape.
//!
//! This path is deliberately separate from `Store::add_artifact`: canonical
//! Zed artifacts must continue to carry their own `pkg/` root. Native package
//! archives use ecosystem-specific roots, so a caller that has independently
//! established an audited native source may opt into this normalizer.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use zed_interfaces::paths::STORE_PKG_DIR;
use zed_interfaces::registry::VersionMetadata;
use zed_lock::{LockClass, LockManager, LockRequest};

use crate::native_artifact_source::NativeArtifactSource;
use crate::store::{Store, require_sha256};

const DEFAULT_MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 200_000;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeArtifactLayout {
    /// The archive already has exactly one top-level `pkg/` directory.
    CanonicalPkg,
    /// Every entry is beneath one top-level directory; strip that directory.
    SingleRoot,
    /// Preserve the archive root and place all top-level entries below `pkg/`.
    ArchiveRoot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeArtifactResult {
    pub package_dir: PathBuf,
    pub layout: NativeArtifactLayout,
    pub entries: usize,
    pub unpacked_bytes: u64,
}

pub(crate) fn add_native_artifact(
    store: &Store,
    archive: &Path,
    expected_sha256: &str,
) -> Result<NativeArtifactResult> {
    return add_native_archive_artifact(store, archive, expected_sha256, true);
}

/// Materialize an artifact only after the caller has classified its exact
/// download URL as one of the protocol-audited native sources.
///
/// The source decides the filesystem policy. We never infer permission from
/// archive shape: NuGet and Python ZIP layouts retain their roots, known
/// source-distribution wrappers may be stripped, and JVM JARs remain JAR
/// files instead of being exploded into a class tree.
pub(crate) fn add_audited_native_artifact(
    store: &Store,
    archive: &Path,
    metadata: &VersionMetadata,
    source: NativeArtifactSource,
) -> Result<PathBuf> {
    let package_dir = match source {
        NativeArtifactSource::MavenCentral | NativeArtifactSource::Clojars => {
            add_native_file_artifact(store, archive, metadata, "jar")?
        }
        NativeArtifactSource::NuGet => {
            add_native_archive_artifact(store, archive, &metadata.sha256, false)?.package_dir
        }
        NativeArtifactSource::PyPi => {
            let strip_single_root = metadata.download_url.ends_with(".tar.gz");
            add_native_archive_artifact(
                store,
                archive,
                &metadata.sha256,
                strip_single_root,
            )?
            .package_dir
        }
        NativeArtifactSource::Npm
        | NativeArtifactSource::CratesIo
        | NativeArtifactSource::GoProxy
        | NativeArtifactSource::Hackage
        | NativeArtifactSource::Cpan
        | NativeArtifactSource::Cran
        | NativeArtifactSource::Jsr
        | NativeArtifactSource::PackagistGithub => {
            add_native_archive_artifact(store, archive, &metadata.sha256, true)?.package_dir
        }
    };
    return Ok(package_dir);
}

fn add_native_archive_artifact(
    store: &Store,
    archive: &Path,
    expected_sha256: &str,
    strip_single_root: bool,
) -> Result<NativeArtifactResult> {
    require_sha256(expected_sha256)?;
    verify_sha256(archive, expected_sha256)?;

    if store.has(expected_sha256) {
        touch_last_used(store, expected_sha256);
        return Ok(NativeArtifactResult {
            package_dir: store.pkg_dir(expected_sha256),
            layout: NativeArtifactLayout::CanonicalPkg,
            entries: 0,
            unpacked_bytes: 0,
        });
    }

    let lock_path = store
        .home()
        .join("locks")
        .join(format!("native-{expected_sha256}.lock"));
    let _lock = LockManager::global().acquire_blocking(
        LockRequest::exclusive(&lock_path)
            .operation(format!("native artifact extraction of {expected_sha256}"))
            .class(LockClass::Artifact)
            .queue_same_process(),
    )?;

    if store.has(expected_sha256) {
        touch_last_used(store, expected_sha256);
        return Ok(NativeArtifactResult {
            package_dir: store.pkg_dir(expected_sha256),
            layout: NativeArtifactLayout::CanonicalPkg,
            entries: 0,
            unpacked_bytes: 0,
        });
    }

    let entry = store.entry_dir(expected_sha256);
    let parent = entry.parent().context("native store entry has a parent")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let raw = staging.path().join("raw");
    fs::create_dir(&raw)?;

    let stats = extract_native_archive(archive, &raw)?;
    let layout = normalize_root(&raw, staging.path(), strip_single_root)?;
    let package_dir = staging.path().join(STORE_PKG_DIR);
    if !package_dir.is_dir() {
        bail!("native artifact normalization did not produce `{STORE_PKG_DIR}/`");
    }
    if raw.exists() {
        fs::remove_dir_all(&raw)?;
    }

    let staging_path = staging.keep();
    match fs::rename(&staging_path, &entry) {
        Ok(()) => {}
        Err(_) if entry.exists() => {
            let _ = fs::remove_dir_all(&staging_path);
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_path);
            return Err(error).with_context(|| {
                format!("publishing native artifact store entry {}", entry.display())
            });
        }
    }

    touch_last_used(store, expected_sha256);
    Ok(NativeArtifactResult {
        package_dir: store.pkg_dir(expected_sha256),
        layout,
        entries: stats.entries,
        unpacked_bytes: stats.unpacked_bytes,
    })
}

fn add_native_file_artifact(
    store: &Store,
    archive: &Path,
    metadata: &VersionMetadata,
    extension: &str,
) -> Result<PathBuf> {
    require_sha256(&metadata.sha256)?;
    verify_sha256(archive, &metadata.sha256)?;
    if store.has(&metadata.sha256) {
        touch_last_used(store, &metadata.sha256);
        return Ok(store.pkg_dir(&metadata.sha256));
    }

    let lock_path = store
        .home()
        .join("locks")
        .join(format!("native-{}.lock", metadata.sha256));
    let _lock = LockManager::global().acquire_blocking(
        LockRequest::exclusive(&lock_path)
            .operation(format!("native file materialization of {}", metadata.sha256))
            .class(LockClass::Artifact)
            .queue_same_process(),
    )?;
    if store.has(&metadata.sha256) {
        touch_last_used(store, &metadata.sha256);
        return Ok(store.pkg_dir(&metadata.sha256));
    }

    let entry = store.entry_dir(&metadata.sha256);
    let parent = entry.parent().context("native store entry has a parent")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let package_dir = staging.path().join(STORE_PKG_DIR);
    fs::create_dir(&package_dir)?;

    let filename = format!("{}-{}.{}", metadata.name, metadata.version, extension);
    if filename.contains('/') || filename.contains('\\') {
        bail!("native file artifact produced an unsafe filename");
    }
    fs::copy(archive, package_dir.join(filename))?;

    let staging_path = staging.keep();
    match fs::rename(&staging_path, &entry) {
        Ok(()) => {}
        Err(_) if entry.exists() => {
            let _ = fs::remove_dir_all(&staging_path);
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&staging_path);
            return Err(error).with_context(|| {
                format!("publishing native file store entry {}", entry.display())
            });
        }
    }

    touch_last_used(store, &metadata.sha256);
    return Ok(store.pkg_dir(&metadata.sha256));
}

#[derive(Debug, Clone, Copy)]
struct ExtractionStats {
    entries: usize,
    unpacked_bytes: u64,
}

fn max_unpacked_bytes() -> u64 {
    std::env::var("ZED_PKG_MAX_UNPACKED_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_MAX_UNPACKED_BYTES)
}

fn verify_sha256(path: &Path, expected_sha256: &str) -> Result<()> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("opening native artifact {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual != expected_sha256 {
        bail!(
            "native artifact hash mismatch: expected {expected_sha256}, got {actual} ({})",
            path.display()
        );
    }
    Ok(())
}

fn extract_native_archive(archive: &Path, destination: &Path) -> Result<ExtractionStats> {
    let mut file = fs::File::open(archive)?;
    let mut magic = [0_u8; 4];
    let read = file.read(&mut magic)?;
    file.rewind()?;

    if read >= 2 && magic[..2] == [0x1f, 0x8b] {
        return extract_tar_gz(file, destination);
    }
    if read == 4 && magic == [b'P', b'K', 0x03, 0x04] {
        return extract_zip(file, destination);
    }
    bail!(
        "unsupported native artifact format in {}; expected gzip-compressed tar or zip",
        archive.display()
    )
}

fn extract_tar_gz(file: fs::File, destination: &Path) -> Result<ExtractionStats> {
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut entries = 0_usize;
    let mut unpacked_bytes = 0_u64;

    for item in archive.entries()? {
        let mut item = item?;
        entries = entries.checked_add(1).context("native tar entry count overflow")?;
        if entries > MAX_ARCHIVE_ENTRIES {
            bail!("native artifact has more than {MAX_ARCHIVE_ENTRIES} entries");
        }

        let entry_type = item.header().entry_type();
        if !(entry_type.is_file() || entry_type.is_dir()) {
            bail!("native tar contains a link or special filesystem entry");
        }
        let path = item.path()?.into_owned();
        let relative = safe_relative_path(&path)?;
        if relative.as_os_str().is_empty() {
            continue;
        }

        if entry_type.is_dir() {
            fs::create_dir_all(destination.join(relative))?;
            continue;
        }

        let size = item.size();
        unpacked_bytes = unpacked_bytes
            .checked_add(size)
            .context("native tar expanded size overflow")?;
        if unpacked_bytes > max_unpacked_bytes() {
            bail!("native artifact exceeds maximum unpacked byte limit");
        }

        let output = destination.join(relative);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .with_context(|| format!("creating native tar entry {}", output.display()))?;
        std::io::copy(&mut item, &mut target)?;
        set_executable_if_requested(&output, item.header().mode().unwrap_or(0))?;
    }

    Ok(ExtractionStats {
        entries,
        unpacked_bytes,
    })
}

fn extract_zip<R: Read + Seek>(reader: R, destination: &Path) -> Result<ExtractionStats> {
    let mut archive = zip::ZipArchive::new(reader)?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("native artifact has more than {MAX_ARCHIVE_ENTRIES} entries");
    }

    let mut unpacked_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut item = archive.by_index(index)?;
        if is_zip_symlink_or_special(item.unix_mode()) {
            bail!("native zip contains a link or special filesystem entry");
        }
        let enclosed = item
            .enclosed_name()
            .context("native zip entry escapes the archive root")?
            .to_path_buf();
        let relative = safe_relative_path(&enclosed)?;
        if relative.as_os_str().is_empty() {
            continue;
        }

        let output = destination.join(relative);
        if item.is_dir() {
            fs::create_dir_all(&output)?;
            continue;
        }
        if !item.is_file() {
            bail!("native zip contains an unsupported entry type");
        }

        unpacked_bytes = unpacked_bytes
            .checked_add(item.size())
            .context("native zip expanded size overflow")?;
        if unpacked_bytes > max_unpacked_bytes() {
            bail!("native artifact exceeds maximum unpacked byte limit");
        }
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .with_context(|| format!("creating native zip entry {}", output.display()))?;
        std::io::copy(&mut item, &mut target)?;
        set_executable_if_requested(&output, item.unix_mode().unwrap_or(0))?;
    }

    Ok(ExtractionStats {
        entries: archive.len(),
        unpacked_bytes,
    })
}

fn normalize_root(
    raw: &Path,
    staging: &Path,
    strip_single_root: bool,
) -> Result<NativeArtifactLayout> {
    let mut top_level = fs::read_dir(raw)?
        .map(|entry| entry.map(|value| value.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    top_level.sort();
    if top_level.is_empty() {
        bail!("native artifact is empty");
    }

    if top_level.len() == 1
        && top_level[0]
            .file_name()
            .is_some_and(|name| name == STORE_PKG_DIR)
        && top_level[0].is_dir()
    {
        fs::rename(&top_level[0], staging.join(STORE_PKG_DIR))?;
        return Ok(NativeArtifactLayout::CanonicalPkg);
    }

    let package = staging.join(STORE_PKG_DIR);
    fs::create_dir(&package)?;

    if strip_single_root && top_level.len() == 1 && top_level[0].is_dir() {
        move_directory_contents(&top_level[0], &package)?;
        return Ok(NativeArtifactLayout::SingleRoot);
    }

    for path in top_level {
        let name = path
            .file_name()
            .context("native archive top-level entry has a file name")?;
        fs::rename(&path, package.join(name))?;
    }
    Ok(NativeArtifactLayout::ArchiveRoot)
}

fn move_directory_contents(source: &Path, destination: &Path) -> Result<()> {
    let mut entries = fs::read_dir(source)?
        .map(|entry| entry.map(|value| value.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        let name = path
            .file_name()
            .context("native archive root entry has a file name")?;
        fs::rename(&path, destination.join(name))?;
    }
    fs::remove_dir(source)?;
    Ok(())
}

fn safe_relative_path(path: &Path) -> Result<PathBuf> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => output.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("native artifact contains an unsafe path: {}", path.display());
            }
        }
    }
    Ok(output)
}

fn is_zip_symlink_or_special(mode: Option<u32>) -> bool {
    let Some(mode) = mode else {
        return false;
    };
    let file_type = mode & 0o170000;
    file_type != 0 && file_type != 0o100000 && file_type != 0o040000
}

#[cfg(unix)]
fn set_executable_if_requested(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if mode & 0o111 == 0 {
        return Ok(());
    }
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable_if_requested(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn touch_last_used(store: &Store, sha256: &str) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let _ = fs::write(store.entry_dir(sha256).join(".last-used"), stamp.to_string());
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn digest(path: &Path) -> String {
        let bytes = fs::read(path).unwrap();
        hex::encode(Sha256::digest(bytes))
    }

    fn write_tar_gz(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, *name, *bytes).unwrap();
        }
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o644);
        for (name, bytes) in entries {
            zip.start_file(*name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn strips_one_native_archive_root_without_changing_store_identity() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("npm.tgz");
        write_tar_gz(
            &archive,
            &[("package/package.json", br#"{"name":"demo"}"#), ("package/index.js", b"ok")],
        );
        let sha = digest(&archive);
        let store = Store::new(&temp.path().join("home"));
        let result = add_native_artifact(&store, &archive, &sha).unwrap();
        assert_eq!(result.layout, NativeArtifactLayout::SingleRoot);
        assert_eq!(fs::read_to_string(result.package_dir.join("index.js")).unwrap(), "ok");
        assert!(result.package_dir.join("package.json").is_file());
        assert_eq!(store.entry_dir(&sha).file_name().unwrap(), sha.as_str());
    }

    #[test]
    fn wraps_flat_jar_or_nupkg_layout_under_pkg() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("artifact.zip");
        write_zip(&archive, &[("META-INF/MANIFEST.MF", b"manifest"), ("demo.class", b"class")]);
        let sha = digest(&archive);
        let store = Store::new(&temp.path().join("home"));
        let result = add_native_artifact(&store, &archive, &sha).unwrap();
        assert_eq!(result.layout, NativeArtifactLayout::ArchiveRoot);
        assert!(result.package_dir.join("META-INF/MANIFEST.MF").is_file());
        assert!(result.package_dir.join("demo.class").is_file());
    }

    #[test]
    fn preserves_an_already_canonical_pkg_root() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("canonical.tgz");
        write_tar_gz(&archive, &[("pkg/file.txt", b"hello")]);
        let sha = digest(&archive);
        let store = Store::new(&temp.path().join("home"));
        let result = add_native_artifact(&store, &archive, &sha).unwrap();
        assert_eq!(result.layout, NativeArtifactLayout::CanonicalPkg);
        assert_eq!(fs::read_to_string(result.package_dir.join("file.txt")).unwrap(), "hello");
    }

    #[test]
    fn rejects_path_traversal_before_publication() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("bad.tgz");
        let file = fs::File::create(&archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();
        // tar crate itself rejects traversal on normal append paths; write a
        // raw GNU header name to ensure our extraction boundary sees it.
        header.as_mut_bytes()[0..11].copy_from_slice(b"../evil.txt");
        header.set_cksum();
        tar.append(&header, &b"bad"[..]).unwrap();
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let sha = digest(&archive);
        let store = Store::new(&temp.path().join("home"));
        assert!(add_native_artifact(&store, &archive, &sha).is_err());
        assert!(!store.has(&sha));
    }

    #[test]
    fn rejects_hash_mismatch_before_extraction() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("artifact.zip");
        write_zip(&archive, &[("package/file.txt", b"hello")]);
        let store = Store::new(&temp.path().join("home"));
        assert!(add_native_artifact(&store, &archive, &"0".repeat(64)).is_err());
    }

    #[test]
    fn rejects_zip_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("symlink.zip");
        let file = fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o120777);
        zip.start_file("package/link", options).unwrap();
        zip.write_all(b"target").unwrap();
        zip.finish().unwrap();
        let sha = digest(&archive);
        let store = Store::new(&temp.path().join("home"));
        assert!(add_native_artifact(&store, &archive, &sha).is_err());
    }

    #[test]
    fn rejects_unsupported_container_magic() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("artifact.gem");
        fs::write(&archive, b"not a zip or gzip tar").unwrap();
        let sha = digest(&archive);
        let store = Store::new(&temp.path().join("home"));
        assert!(add_native_artifact(&store, &archive, &sha).is_err());
    }

    #[test]
    fn top_level_entries_are_deterministic() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("flat.zip");
        write_zip(&archive, &[("b.txt", b"b"), ("a.txt", b"a")]);
        let sha = digest(&archive);
        let store = Store::new(&temp.path().join("home"));
        let result = add_native_artifact(&store, &archive, &sha).unwrap();
        let names = fs::read_dir(result.package_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(names, BTreeSet::from(["a.txt".to_string(), "b.txt".to_string()]));
    }
}
