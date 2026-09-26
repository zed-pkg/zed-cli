//! Normalize artifacts served directly by ecosystem registries into the Zed
//! store layout without changing the bytes (or digest) the registry attested.
//!
//! Zed's own published archives are rooted at `pkg/`. Native registries are
//! not: npm uses `package/`, Cargo crates commonly use `name-version/`, Go
//! module zips use `module@version/`, and wheels/NuGet archives can have more
//! than one root entry. The cache must continue to pin the upstream bytes, so
//! repacking is the wrong fix. Instead, verify the upstream digest first and
//! normalize only the extracted store tree.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zed_interfaces::registry::VersionMetadata;

use crate::pack::sha256_file;
use crate::store::{Store, extract_archive_for_update};

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
}

fn native_layout(version: &VersionMetadata) -> Option<NativeLayout> {
    let url = reqwest::Url::parse(&version.download_url).ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if NATIVE_ARCHIVE_HOSTS.contains(&host.as_str()) {
        return Some(NativeLayout::Archive);
    }
    if NATIVE_FILE_HOSTS.contains(&host.as_str()) && url.path().ends_with(".jar") {
        return Some(NativeLayout::Jar);
    }
    None
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
                format!(
                    "extracting native registry artifact {}",
                    archive.display()
                )
            })?;
            normalize_extracted_tree(&unpacked, &package_root)?;
        }
        NativeLayout::Jar => {
            fs::create_dir_all(&package_root)?;
            let filename = format!("{}-{}.jar", version.name, version.version);
            fs::copy(archive, package_root.join(filename))?;
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
            return Err(error).with_context(|| {
                format!("publishing native artifact into {}", entry.display())
            });
        }
    }
    Ok(Some(store.pkg_dir(&version.sha256)))
}

/// Strip one conventional archive root when there is exactly one top-level
/// directory; otherwise preserve every top-level entry under `pkg/`.
///
/// That single rule covers npm/Cargo/Go/sdists while keeping wheels and NuGet
/// packages intact. The hardened extractor has already rejected traversal,
/// links, special files, entry-count abuse, and decompression bombs.
fn normalize_extracted_tree(unpacked: &Path, package_root: &Path) -> Result<()> {
    let mut entries = fs::read_dir(unpacked)?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    if entries.is_empty() {
        bail!("native artifact archive is empty");
    }

    if entries.len() == 1 && entries[0].file_type()?.is_dir() {
        fs::rename(entries[0].path(), package_root)?;
        fs::remove_dir(unpacked)?;
        return Ok(());
    }

    fs::create_dir_all(package_root)?;
    for entry in entries {
        fs::rename(entry.path(), package_root.join(entry.file_name()))?;
    }
    fs::remove_dir(unpacked)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tar::Builder;
    use zed_interfaces::artifact::ArtifactFormat;

    use super::*;

    fn native_version(url: &str, sha256: String) -> VersionMetadata {
        VersionMetadata {
            org: "npm".to_string(),
            name: "left-pad".to_string(),
            version: "1.3.0".to_string(),
            sha256,
            size: 0,
            format: ArtifactFormat::TarGz,
            vcs_tag: "1.3.0".to_string(),
            vcs_commit: None,
            download_url: url.to_string(),
            published_at: "1970-01-01T00:00:00Z".to_string(),
            yanked: false,
            mirrors: Vec::new(),
            signatures: Vec::new(),
        }
    }

    fn npm_style_archive(path: &Path) {
        let file = fs::File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut tar = Builder::new(encoder);
        let bytes = b"module.exports = 1;\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "package/index.js", &bytes[..])
            .unwrap();
        tar.finish().unwrap();
    }

    #[test]
    fn npm_single_root_is_normalized_without_changing_the_pinned_digest() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("left-pad.tgz");
        npm_style_archive(&archive);
        let (sha256, _) = sha256_file(&archive).unwrap();
        let store = Store::new(&temp.path().join("home"));
        let version = native_version(
            "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
            sha256.clone(),
        );

        let package = add_if_native(&store, &archive, &version)
            .unwrap()
            .expect("npm is an admitted native source");
        assert_eq!(package, store.pkg_dir(&sha256));
        assert!(package.join("index.js").is_file());
        assert!(!package.join("package").exists());
    }

    #[test]
    fn arbitrary_https_hosts_do_not_weaken_the_zed_archive_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("package.tgz");
        npm_style_archive(&archive);
        let (sha256, _) = sha256_file(&archive).unwrap();
        let store = Store::new(&temp.path().join("home"));
        let version = native_version("https://example.invalid/package.tgz", sha256);
        assert!(add_if_native(&store, &archive, &version).unwrap().is_none());
    }

    #[test]
    fn multi_root_zip_is_wrapped_under_pkg_instead_of_dropping_entries() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("package.nupkg");
        {
            let file = fs::File::create(&archive).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("lib/a.dll", options).unwrap();
            zip.write_all(b"dll").unwrap();
            zip.start_file("package.nuspec", options).unwrap();
            zip.write_all(b"metadata").unwrap();
            zip.finish().unwrap();
        }
        let (sha256, _) = sha256_file(&archive).unwrap();
        let store = Store::new(&temp.path().join("home"));
        let mut version = native_version(
            "https://api.nuget.org/v3-flatcontainer/acme/1.0.0/acme.1.0.0.nupkg",
            sha256.clone(),
        );
        version.format = ArtifactFormat::Zip;

        let package = add_if_native(&store, &archive, &version)
            .unwrap()
            .expect("NuGet is an admitted native source");
        assert!(package.join("lib/a.dll").is_file());
        assert!(package.join("package.nuspec").is_file());
    }
}
