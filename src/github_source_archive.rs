//! Normalize GitHub-generated repository archives into canonical Zed artifacts.
//!
//! GitHub's automatic tag archives are source archives: they are rooted under
//! `<repo>-<ref>/` and are not directly installable by Zed, whose immutable
//! store accepts only archives rooted under `pkg/`. This module deliberately
//! keeps the hardened store extractor as the final extraction boundary and the
//! ordinary deterministic packer as the output path, so direct GitHub fallback
//! has the same package layout, ignore rules, executable-bit normalization, and
//! digest semantics as `zed pack`.

use std::fs;
use std::io::{Read, sink};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::registry::VersionMetadata;
use zed_interfaces::source::{GithubIdentity, parse_github_identity};

// GitHub's generated tarballs currently include a POSIX PAX global metadata
// member before the repository tree. Canonical Zed artifacts intentionally
// reject every non-file/non-directory member, so source fallback strips only
// this metadata-only record into a temporary tarball and then still sends the
// result through the ordinary hardened extractor. Keep preprocessing bounded so
// it cannot become a decompression-bomb bypass around the store extractor.
const MAX_GITHUB_SOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_GITHUB_SOURCE_ENTRIES: usize = 200_000;

/// Extract one GitHub-generated source archive, validate the tagged manifest,
/// and deterministically repack it into the artifact format expected by the
/// registry/store boundary.
///
/// If `version.sha256` is already pinned, the normalized bytes MUST reproduce
/// that digest (and declared size, when present). This keeps source fallback
/// useful for frozen installs without weakening lockfile integrity.
pub(crate) fn repack_github_archive(
    source_archive: &Path,
    destination: &Path,
    identity: &GithubIdentity,
    version: &VersionMetadata,
) -> Result<()> {
    let extracted = tempfile::tempdir().context("create GitHub source extraction directory")?;
    let sanitized = strip_github_pax_global_headers(source_archive)?;
    let archive_for_extract = sanitized
        .as_ref()
        .map(tempfile::NamedTempFile::path)
        .unwrap_or(source_archive);
    crate::store::extract_archive_for_update(archive_for_extract, extracted.path())
        .context("extract GitHub source archive")?;

    repack_extracted_root(extracted.path(), destination, identity, version)
}

/// GitHub/codeload tarballs may carry a metadata-only POSIX PAX global header
/// (`typeflag = 'g'`). `tar` deliberately exposes global headers as entries,
/// while it consumes local PAX/GNU long-name records as metadata for the file
/// they describe. Iterate raw entries here so every other header is preserved
/// byte-for-byte semantically; remove only the global record, then let the
/// strict store extractor perform path/type/size validation on the result.
fn strip_github_pax_global_headers(
    source_archive: &Path,
) -> Result<Option<tempfile::NamedTempFile>> {
    let mut magic = [0u8; 2];
    let read = {
        let mut file = fs::File::open(source_archive)?;
        file.read(&mut magic).unwrap_or(0)
    };
    if read < 2 || magic != [0x1f, 0x8b] {
        return Ok(None);
    }

    let input = fs::File::open(source_archive)?;
    let mut archive = tar::Archive::new(GzDecoder::new(input));
    let sanitized = tempfile::NamedTempFile::new()
        .context("create sanitized GitHub source archive")?;
    let output = sanitized
        .reopen()
        .context("reopen sanitized GitHub source archive")?;
    let encoder = GzEncoder::new(output, Compression::default());
    let mut builder = tar::Builder::new(encoder);

    let mut bytes = 0u64;
    let mut count = 0usize;
    for raw_entry in archive.entries()?.raw(true) {
        let mut entry = raw_entry.context("read GitHub source tar entry")?;
        count += 1;
        ensure!(
            count <= MAX_GITHUB_SOURCE_ENTRIES,
            "GitHub source archive has more than {MAX_GITHUB_SOURCE_ENTRIES} raw entries; refusing"
        );
        let size = entry.header().size().context("read GitHub tar entry size")?;
        bytes = bytes.saturating_add(size);
        ensure!(
            bytes <= MAX_GITHUB_SOURCE_BYTES,
            "GitHub source archive expands past the {MAX_GITHUB_SOURCE_BYTES}-byte preprocessing cap; refusing"
        );

        if entry.header().entry_type() == tar::EntryType::XGlobalHeader {
            // Drain the record so malformed/truncated metadata still fails the
            // source read instead of being silently accepted.
            let copied = std::io::copy(&mut entry, &mut sink())?;
            ensure!(
                copied == size,
                "GitHub PAX global header is truncated: declared {size} bytes, read {copied}"
            );
            continue;
        }

        let header = entry.header().clone();
        builder
            .append(&header, &mut entry)
            .context("copy GitHub source tar entry into sanitized archive")?;
    }

    let encoder = builder
        .into_inner()
        .context("finish sanitized GitHub source tar")?;
    encoder
        .finish()
        .context("finish sanitized GitHub source gzip stream")?;
    Ok(Some(sanitized))
}

fn repack_extracted_root(
    extracted: &Path,
    destination: &Path,
    identity: &GithubIdentity,
    version: &VersionMetadata,
) -> Result<()> {
    let source_root = github_source_root(extracted)?;
    let manifest_path = source_root.join(MANIFEST_FILE);
    let manifest_text = fs::read_to_string(&manifest_path).with_context(|| {
        format!(
            "GitHub source archive for {}/{}@{} is missing readable {MANIFEST_FILE}",
            version.org, version.name, version.version
        )
    })?;
    let manifest = Manifest::parse(&manifest_text).map_err(|error| anyhow!(error))?;
    validate_manifest(&manifest, identity, version)?;

    let packed_dir = tempfile::tempdir().context("create normalized GitHub package directory")?;
    let packed = crate::pack::pack_format(
        &source_root,
        &manifest,
        Some(packed_dir.path()),
        version.format,
    )
    .context("repack GitHub source archive as a canonical Zed artifact")?;

    let has_pinned_digest = zed_interfaces::manifest::is_sha256_hex(&version.sha256);
    if has_pinned_digest {
        ensure!(
            packed.sha256 == version.sha256,
            "GitHub source archive normalized to sha256 {}, but the package metadata pins {}; refusing non-identical fallback",
            packed.sha256,
            version.sha256
        );
        if version.size > 0 {
            ensure!(
                packed.size == version.size,
                "GitHub source archive normalized to {} bytes, but the package metadata pins {} bytes; refusing non-identical fallback",
                packed.size,
                version.size
            );
        }
    }

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&packed.path, destination).with_context(|| {
        format!(
            "copy normalized GitHub artifact to {}",
            destination.display()
        )
    })?;
    Ok(())
}

/// GitHub/codeload archives contain exactly one synthetic top-level directory.
/// Requiring that shape prevents an archive from smuggling unrelated roots
/// into the package before the normal packer's include/exclude policy runs.
fn github_source_root(extracted: &Path) -> Result<PathBuf> {
    let mut entries = fs::read_dir(extracted)
        .with_context(|| format!("read extracted GitHub archive at {}", extracted.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    ensure!(
        entries.len() == 1,
        "GitHub source archive must contain exactly one top-level directory, found {} entries",
        entries.len()
    );
    let root = entries.remove(0).path();
    ensure!(
        fs::symlink_metadata(&root)?.file_type().is_dir(),
        "GitHub source archive top-level entry must be a directory"
    );
    Ok(root)
}

fn validate_manifest(
    manifest: &Manifest,
    identity: &GithubIdentity,
    version: &VersionMetadata,
) -> Result<()> {
    ensure!(
        manifest.package.org == version.org
            && manifest.package.name == version.name
            && manifest.package.version == version.version,
        "tagged {MANIFEST_FILE} claims {}/{}@{}, expected {}/{}@{}",
        manifest.package.org,
        manifest.package.name,
        manifest.package.version,
        version.org,
        version.name,
        version.version
    );
    let declared = parse_github_identity(&manifest.package.repository.url).with_context(|| {
        format!(
            "tagged {MANIFEST_FILE} repository `{}` is not a GitHub repository",
            manifest.package.repository.url
        )
    })?;
    ensure!(
        declared.owner.eq_ignore_ascii_case(&identity.owner)
            && declared.repo.eq_ignore_ascii_case(&identity.repo),
        "tagged {MANIFEST_FILE} points at {}, expected {}",
        declared.web_url(),
        identity.web_url()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::artifact::ArtifactFormat;

    fn identity() -> GithubIdentity {
        GithubIdentity {
            owner: "acme".into(),
            repo: "http-kit".into(),
        }
    }

    fn version(format: ArtifactFormat) -> VersionMetadata {
        VersionMetadata {
            org: "acme".into(),
            name: "http-kit".into(),
            version: "1.2.0".into(),
            sha256: String::new(),
            size: 0,
            format,
            vcs_tag: "v1.2.0".into(),
            vcs_commit: None,
            download_url: "https://github.com/acme/http-kit/archive/refs/tags/v1.2.0.tar.gz".into(),
            published_at: "1970-01-01T00:00:00Z".into(),
            yanked: false,
            mirrors: Vec::new(),
            signatures: Vec::new(),
        }
    }

    fn extracted_source() -> tempfile::TempDir {
        let extracted = tempfile::tempdir().unwrap();
        let root = extracted.path().join("http-kit-deadbeef");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join(MANIFEST_FILE),
            r#"[package]
org = "acme"
name = "http-kit"
version = "1.2.0"
language = "rust"

[package.repository]
vcs = "git"
url = "https://github.com/acme/http-kit"
"#,
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn answer() -> u8 { 42 }\n").unwrap();
        extracted
    }

    fn pax_record(key: &str, value: &str) -> Vec<u8> {
        let payload = format!("{key}={value}\n");
        let mut digits = 1usize;
        loop {
            let len = digits + 1 + payload.len();
            let next_digits = len.to_string().len();
            if next_digits == digits {
                return format!("{len} {payload}").into_bytes();
            }
            digits = next_digits;
        }
    }

    fn github_style_tar_with_global_pax() -> tempfile::NamedTempFile {
        let source = extracted_source();
        let root = source.path().join("http-kit-deadbeef");
        let archive = tempfile::NamedTempFile::new().unwrap();
        let file = archive.reopen().unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);

        let pax = pax_record("comment", "deadbeef");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::XGlobalHeader);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(pax.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, "pax_global_header", pax.as_slice())
            .unwrap();
        builder
            .append_dir_all("http-kit-deadbeef", &root)
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        archive
    }

    #[test]
    fn github_source_tree_becomes_store_compatible_tar_and_zip() {
        for format in [ArtifactFormat::TarGz, ArtifactFormat::Zip] {
            let extracted = extracted_source();
            let output = tempfile::NamedTempFile::new().unwrap();
            let metadata = version(format);
            repack_extracted_root(extracted.path(), output.path(), &identity(), &metadata).unwrap();

            let (sha256, _) = crate::pack::sha256_file(output.path()).unwrap();
            let store_home = tempfile::tempdir().unwrap();
            let store = crate::store::Store::new(store_home.path());
            let package = store.add_artifact(output.path(), &sha256).unwrap();
            assert!(package.join(MANIFEST_FILE).is_file());
            assert!(package.join("src/lib.rs").is_file());
        }
    }

    #[test]
    fn github_pax_global_header_is_stripped_before_strict_extraction() {
        let source = github_style_tar_with_global_pax();

        // The canonical artifact/update extractor remains strict: raw GitHub
        // metadata is not silently admitted into the immutable package format.
        let strict_dest = tempfile::tempdir().unwrap();
        let strict_error = crate::store::extract_archive_for_update(source.path(), strict_dest.path())
            .unwrap_err()
            .to_string();
        assert!(strict_error.contains("XGlobalHeader"));

        let output = tempfile::NamedTempFile::new().unwrap();
        repack_github_archive(
            source.path(),
            output.path(),
            &identity(),
            &version(ArtifactFormat::TarGz),
        )
        .unwrap();

        let (sha256, _) = crate::pack::sha256_file(output.path()).unwrap();
        let store_home = tempfile::tempdir().unwrap();
        let store = crate::store::Store::new(store_home.path());
        let package = store.add_artifact(output.path(), &sha256).unwrap();
        assert!(package.join(MANIFEST_FILE).is_file());
        assert!(package.join("src/lib.rs").is_file());
    }

    #[test]
    fn pinned_digest_must_match_normalized_bytes() {
        let extracted = extracted_source();
        let output = tempfile::NamedTempFile::new().unwrap();
        let mut metadata = version(ArtifactFormat::TarGz);
        metadata.sha256 = "ab".repeat(32);
        metadata.size = 1;
        let error = repack_extracted_root(extracted.path(), output.path(), &identity(), &metadata)
            .unwrap_err()
            .to_string();
        assert!(error.contains("refusing non-identical fallback"));
    }

    #[test]
    fn tagged_manifest_must_match_requested_package_identity() {
        let extracted = extracted_source();
        let root = extracted.path().join("http-kit-deadbeef");
        let text = fs::read_to_string(root.join(MANIFEST_FILE)).unwrap();
        fs::write(
            root.join(MANIFEST_FILE),
            text.replace("version = \"1.2.0\"", "version = \"9.9.9\""),
        )
        .unwrap();
        let output = tempfile::NamedTempFile::new().unwrap();
        let error = repack_extracted_root(
            extracted.path(),
            output.path(),
            &identity(),
            &version(ArtifactFormat::TarGz),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("expected acme/http-kit@1.2.0"));
    }
}
