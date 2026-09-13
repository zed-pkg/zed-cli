//! Normalize GitHub-generated repository archives into canonical Zed artifacts.
//!
//! GitHub's automatic tag archives are source archives: they are rooted under
//! `<repo>-<ref>/` and are not directly installable by Zed, whose immutable
//! store accepts only archives rooted under `pkg/`. This module deliberately
//! reuses the hardened store extractor and the ordinary deterministic packer so
//! direct GitHub fallback has exactly the same package layout, ignore rules,
//! executable-bit normalization, and digest semantics as `zed pack`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::registry::VersionMetadata;
use zed_interfaces::source::{GithubIdentity, parse_github_identity};

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
    crate::store::extract_archive_for_update(source_archive, extracted.path())
        .context("extract GitHub source archive")?;

    repack_extracted_root(extracted.path(), destination, identity, version)
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
