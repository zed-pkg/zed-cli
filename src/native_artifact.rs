//! Normalize artifacts served directly by ecosystem registries into the Zed
//! store layout without changing the bytes (or digest) the registry attested.
//!
//! Zed's own published archives are rooted at `pkg/`. Native registries are
//! not: npm uses `package/`, Cargo crates commonly use `name-version/`, Go
//! module zips use `module@version/`, and wheels/NuGet archives can have more
//! than one root entry. RubyGems and Hex add another layer by shipping a tar
//! envelope whose package payload is itself a gzip-compressed tar. The cache
//! continues to pin the exact upstream bytes; only the extracted store tree is
//! normalized.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::registry::VersionMetadata;

use crate::pack::sha256_file;
use crate::store::{Store, extract_archive_for_update};

const MAX_NATIVE_ENVELOPE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_NATIVE_ENVELOPE_ENTRIES: usize = 4096;

/// Hosts whose public package artifacts the Cloudflare fallback is allowed to
/// surface directly. Keep this list finite: a registry response must not turn
/// the installer into a generic URL/extraction proxy.
const NATIVE_ARCHIVE_HOSTS: &[&str] = &[
    "registry.npmjs.org",
    "static.crates.io",
    "crates.io",
    "files.pythonhosted.org",
    "api.nuget.org",
    "globalcdn.nuget.org",
    "proxy.golang.org",
    "hackage.haskell.org",
    "cpan.metacpan.org",
    "cran.r-project.org",
    "npm.jsr.io",
];

/// JVM repositories serve jars, which are ZIP containers but must remain jar
/// files for the Java adapter/classpath. Extracting them would destroy the
/// package-manager representation even though the bytes are technically ZIP.
const NATIVE_FILE_HOSTS: &[&str] = &[
    "repo.maven.apache.org",
    "repo1.maven.org",
    "repo.clojars.org",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeLayout {
    Archive,
    Jar,
    RubyGem,
    HexPackage,
}

fn native_layout(version: &VersionMetadata) -> Option<NativeLayout> {
    let url = reqwest::Url::parse(&version.download_url).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
    {
        return None;
    }

    let host = url.host_str()?.to_ascii_lowercase();
    if NATIVE_ARCHIVE_HOSTS.contains(&host.as_str()) {
        return Some(NativeLayout::Archive);
    }
    if NATIVE_FILE_HOSTS.contains(&host.as_str()) && url.path().ends_with(".jar") {
        return Some(NativeLayout::Jar);
    }
    if host == "rubygems.org" && url.path().ends_with(".gem") {
        return Some(NativeLayout::RubyGem);
    }
    if (host == "repo.hex.pm" || host == "hex.pm") && url.path().ends_with(".tar") {
        return Some(NativeLayout::HexPackage);
    }
    return None;
}

/// Add a direct native-registry artifact to the immutable store when its URL
/// belongs to an explicitly admitted upstream.
///
/// `Ok(None)` means this is an ordinary Zed archive and the caller must use
/// `Store::add_artifact`, preserving the existing strict `pkg/` invariant.
pub(crate) fn add_if_native(
    store: &Store,
    archive: &Path,
    version: &VersionMetadata,
) -> Result<Option<PathBuf>> {
    let Some(layout) = native_layout(version) else {
        return Ok(None);
    };

    let (actual, _) = sha256_file(archive)?;
    if actual != version.sha256 {
        bail!(
            "native artifact hash mismatch: expected {}, got {} ({})",
            version.sha256,
            actual,
            archive.display()
        );
    }
    if store.has(&version.sha256) {
        return Ok(Some(store.pkg_dir(&version.sha256)));
    }

    let entry = store.entry_dir(&version.sha256);
    let parent = entry.parent().context("native store entry has a parent")?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::tempdir_in(parent)?;
    let package_root = temporary.path().join("pkg");

    match layout {
        NativeLayout::Archive => {
            let unpacked = temporary.path().join("unpacked");
            fs::create_dir_all(&unpacked)?;
            extract_archive_for_update(archive, &unpacked).with_context(|| {
                format!("extracting native registry artifact {}", archive.display())
            })?;
            let root = conventional_archive_root(version);
            normalize_extracted_tree(&unpacked, &package_root, root.as_deref())?;
        }
        NativeLayout::Jar => {
            fs::create_dir_all(&package_root)?;
            let filename = format!("{}-{}.jar", version.name, version.version);
            fs::copy(archive, package_root.join(filename))?;
        }
        NativeLayout::RubyGem => {
            extract_nested_tar_gzip(archive, "data.tar.gz", temporary.path(), &package_root)?;
        }
        NativeLayout::HexPackage => {
            extract_nested_tar_gzip(archive, "contents.tar.gz", temporary.path(), &package_root)?;
        }
    }

    if !package_root.is_dir() {
        bail!("native artifact normalization produced no `pkg/` directory");
    }

    // Publish the complete entry atomically. The caller already holds the
    // per-sha artifact-preparation lock; this still handles an entry that was
    // completed by another compatible path before the final rename.
    let temporary_path = temporary.keep();
    match fs::rename(&temporary_path, &entry) {
        Ok(()) => {}
        Err(_) if entry.is_dir() => {
            let _ = fs::remove_dir_all(&temporary_path);
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary_path);
            return Err(error)
                .with_context(|| format!("publishing native artifact into {}", entry.display()));
        }
    }
    return Ok(Some(store.pkg_dir(&version.sha256)));
}

/// RubyGems and Hex both use an uncompressed tar envelope around a single
/// gzip-compressed tar payload. Read only the named regular-file member, bound
/// it by its declared size, then hand the nested archive to the hardened Zed
/// extractor before normalizing its roots.
fn extract_nested_tar_gzip(
    archive: &Path,
    member: &str,
    temporary_root: &Path,
    package_root: &Path,
) -> Result<()> {
    let nested = temporary_root.join("native-payload.tar.gz");
    copy_nested_payload(archive, member, &nested)?;
    let unpacked = temporary_root.join("unpacked");
    fs::create_dir_all(&unpacked)?;
    extract_archive_for_update(&nested, &unpacked)
        .with_context(|| format!("extracting nested native payload `{member}`"))?;
    fs::remove_file(&nested)?;
    // Nested payloads already contain package-relative paths. A sole `lib/`
    // or `src/` directory is package content, not a wrapper to strip.
    normalize_extracted_tree(&unpacked, package_root, None)?;
    return Ok(());
}

fn copy_nested_payload(archive: &Path, member: &str, nested: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    if file.metadata()?.len() > MAX_NATIVE_ENVELOPE_BYTES {
        bail!("native package envelope exceeds the {MAX_NATIVE_ENVELOPE_BYTES}-byte cap");
    }
    let mut outer = tar::Archive::new(file);
    let mut found = false;

    for (index, entry) in outer.entries()?.enumerate() {
        if index >= MAX_NATIVE_ENVELOPE_ENTRIES {
            bail!("native package envelope exceeds the {MAX_NATIVE_ENVELOPE_ENTRIES}-entry cap");
        }
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        if path != Path::new(member) {
            continue;
        }
        if found {
            bail!("native package contains duplicate payload `{member}`");
        }
        if !entry.header().entry_type().is_file() {
            bail!("native package member `{member}` is not a regular file");
        }
        let declared = entry.header().size()?;
        if declared > MAX_NATIVE_ENVELOPE_BYTES {
            bail!("native package payload `{member}` exceeds the byte cap");
        }
        let mut limited = (&mut entry).take(declared.saturating_add(1));
        let mut output = fs::File::create(nested)?;
        let copied = std::io::copy(&mut limited, &mut output)?;
        if copied != declared {
            bail!(
                "native package member `{member}` size mismatch: expected {declared}, got {copied}"
            );
        }
        found = true;
    }

    if !found {
        bail!("native package is missing required payload `{member}`");
    }

    return Ok(());
}

/// Select only a wrapper defined by the source's archive convention. Wheels,
/// NuGet ZIPs, and unrecognized layouts must retain their package-relative paths.
fn conventional_archive_root(version: &VersionMetadata) -> Option<String> {
    let url = reqwest::Url::parse(&version.download_url).ok()?;
    let host = url.host_str()?;
    if ["registry.npmjs.org", "npm.jsr.io"].contains(&host) {
        return Some("package".to_string());
    }
    if ["static.crates.io", "crates.io", "hackage.haskell.org"].contains(&host) {
        return Some(format!("{}-{}", version.name, version.version));
    }
    if host == "files.pythonhosted.org" {
        let filename = url.path().rsplit('/').next()?;
        return filename.strip_suffix(".tar.gz").map(str::to_string);
    }
    return None;
}

/// Strip an explicitly expected wrapper only when it is the sole root entry.
/// The hardened extractor has already rejected traversal and special files.
fn normalize_extracted_tree(
    unpacked: &Path,
    package_root: &Path,
    expected_root: Option<&str>,
) -> Result<()> {
    let mut entries = fs::read_dir(unpacked)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    if entries.is_empty() {
        bail!("native artifact archive is empty");
    }

    if entries.len() == 1
        && entries[0].file_type()?.is_dir()
        && expected_root.is_some_and(|root| entries[0].file_name() == std::ffi::OsStr::new(root))
    {
        fs::rename(entries[0].path(), package_root)?;
        fs::remove_dir(unpacked)?;
        return Ok(());
    }

    fs::create_dir_all(package_root)?;
    for entry in entries {
        fs::rename(entry.path(), package_root.join(entry.file_name()))?;
    }
    fs::remove_dir(unpacked)?;
    return Ok(());
}

#[cfg(test)]
#[path = "native_artifact/tests/mod.rs"]
mod tests;
