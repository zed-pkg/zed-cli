//! Mirror a packed Zed artifact onto GitHub Packages (GHCR) as an OCI
//! artifact so it appears on `https://github.com/orgs/{owner}/packages`.
//!
//! GitHub has no native Zed package type. Container packages on GHCR are the
//! supported surface that shows on the org Packages page. The git tag is the
//! OCI tag; `org.opencontainers.image.source` links the package to the repo.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::source::{
    GithubIdentity, ghcr_blob_url, ghcr_manifest_url, ghcr_reference, ghcr_repository,
    ghcr_uploads_url, github_packages_web_url, parse_github_identity,
};
use zed_interfaces::{
    ArtifactFormat, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor, OciDigest,
    ZED_OCI_CONFIG_MEDIA_TYPE_V1, ZED_OCI_PACKAGE_TAR_GZ_MEDIA_TYPE_V1,
    ZED_OCI_PACKAGE_ZIP_MEDIA_TYPE_V1,
};

use crate::pack::PackResult;
use crate::source_fallback::SourceFallbackConfig;

const OCI_CONFIG_SCHEMA: &str = "zed.oci-config/v1";

/// Push the packed artifact to `ghcr.io/{owner}/{repo}:{tag}` when the
/// package is a GitHub repo and a token is available.
pub fn mirror_packed_ghcr(
    manifest: &Manifest,
    packed: &PackResult,
    vcs_tag: &str,
    vcs_commit: Option<&str>,
) -> Result<GhcrOutcome> {
    if !manifest
        .package
        .artifacts
        .github_packages_enabled(Some(manifest.package.repository.url.as_str()))
    {
        return Ok(GhcrOutcome::Skipped("github_packages disabled"));
    }
    let Some(identity) = parse_github_identity(&manifest.package.repository.url) else {
        return Ok(GhcrOutcome::Skipped("repository is not github.com"));
    };
    let config = SourceFallbackConfig::from_env();
    let Some(token) = config.github_token.as_deref() else {
        eprintln!(
            "warning: GitHub Packages (GHCR) mirror skipped for {} (set GITHUB_TOKEN / GH_TOKEN / ZED_PKG_GITHUB_TOKEN with write:packages)",
            manifest.full_name()
        );
        return Ok(GhcrOutcome::Skipped("no GitHub token"));
    };

    let artifact = build_ghcr_artifact(manifest, packed, vcs_tag, vcs_commit)?;
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let bearer = ghcr_bearer(&client, token, &identity)?;
    upload_blob(&client, &bearer, &identity, &artifact.config)?;
    upload_blob(&client, &bearer, &identity, &artifact.layer)?;
    put_manifest(&client, &bearer, &identity, vcs_tag, &artifact.manifest_bytes)?;
    Ok(GhcrOutcome::Uploaded {
        reference: ghcr_reference(&identity, vcs_tag),
        digest: artifact.manifest_digest,
        web_url: github_packages_web_url(&identity),
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum GhcrOutcome {
    Skipped(&'static str),
    Uploaded {
        reference: String,
        digest: String,
        web_url: String,
    },
}

#[derive(Debug)]
struct Blob {
    digest: String,
    media_type: &'static str,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct GhcrArtifact {
    config: Blob,
    layer: Blob,
    manifest_bytes: Vec<u8>,
    manifest_digest: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ZedOciConfig<'a> {
    schema: &'static str,
    package: ZedOciPackage<'a>,
    repository: &'a str,
    vcs_tag: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    vcs_commit: Option<&'a str>,
    artifact: ZedOciArtifactMeta<'a>,
}

#[derive(Debug, Serialize)]
struct ZedOciPackage<'a> {
    org: &'a str,
    name: &'a str,
    version: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ZedOciArtifactMeta<'a> {
    format: ArtifactFormat,
    digest: &'a str,
    size: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciImageManifest<'a> {
    schema_version: u32,
    media_type: &'static str,
    artifact_type: &'static str,
    config: &'a OciDescriptor,
    layers: Vec<&'a OciDescriptor>,
    annotations: &'a BTreeMap<String, String>,
}

fn build_ghcr_artifact(
    manifest: &Manifest,
    packed: &PackResult,
    vcs_tag: &str,
    vcs_commit: Option<&str>,
) -> Result<GhcrArtifact> {
    let layer_bytes = fs::read(&packed.path)
        .with_context(|| format!("read packed artifact {}", packed.path.display()))?;
    let layer_digest = format!("sha256:{}", packed.sha256);
    if sha256_hex(&layer_bytes) != packed.sha256 {
        bail!(
            "packed artifact {} drifted from recorded sha256 {}",
            packed.path.display(),
            packed.sha256
        );
    }
    let layer_media = package_media_type(packed.format);
    let config_bytes = serde_json::to_vec(&ZedOciConfig {
        schema: OCI_CONFIG_SCHEMA,
        package: ZedOciPackage {
            org: &manifest.package.org,
            name: &manifest.package.name,
            version: &manifest.package.version,
        },
        repository: &manifest.package.repository.url,
        vcs_tag,
        vcs_commit,
        artifact: ZedOciArtifactMeta {
            format: packed.format,
            digest: &layer_digest,
            size: packed.size,
        },
    })?;
    let config_digest = format!("sha256:{}", sha256_hex(&config_bytes));
    let config_descriptor = descriptor(
        ZED_OCI_CONFIG_MEDIA_TYPE_V1,
        &config_digest,
        config_bytes.len() as u64,
    )?;
    let layer_descriptor = descriptor(layer_media, &layer_digest, packed.size)?;
    let mut annotations = BTreeMap::from([
        (
            "org.opencontainers.image.source".to_string(),
            manifest.package.repository.url.clone(),
        ),
        (
            "org.opencontainers.image.title".to_string(),
            manifest.full_name(),
        ),
        (
            "org.opencontainers.image.version".to_string(),
            manifest.package.version.clone(),
        ),
        (
            "org.opencontainers.image.ref.name".to_string(),
            vcs_tag.to_string(),
        ),
        (
            "dev.zed-pkg.package".to_string(),
            manifest.full_name(),
        ),
        ("dev.zed-pkg.vcs-tag".to_string(), vcs_tag.to_string()),
    ]);
    if let Some(commit) = vcs_commit {
        annotations.insert(
            "org.opencontainers.image.revision".to_string(),
            commit.to_string(),
        );
    }
    let manifest_bytes = serde_json::to_vec(&OciImageManifest {
        schema_version: 2,
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE,
        artifact_type: ZED_OCI_CONFIG_MEDIA_TYPE_V1,
        config: &config_descriptor,
        layers: vec![&layer_descriptor],
        annotations: &annotations,
    })?;
    Ok(GhcrArtifact {
        config: Blob {
            digest: config_digest,
            media_type: ZED_OCI_CONFIG_MEDIA_TYPE_V1,
            bytes: config_bytes,
        },
        layer: Blob {
            digest: layer_digest,
            media_type: layer_media,
            bytes: layer_bytes,
        },
        manifest_digest: format!("sha256:{}", sha256_hex(&manifest_bytes)),
        manifest_bytes,
    })
}

fn package_media_type(format: ArtifactFormat) -> &'static str {
    match format {
        ArtifactFormat::TarGz => ZED_OCI_PACKAGE_TAR_GZ_MEDIA_TYPE_V1,
        ArtifactFormat::Zip => ZED_OCI_PACKAGE_ZIP_MEDIA_TYPE_V1,
    }
}

fn descriptor(media_type: &str, digest: &str, size: u64) -> Result<OciDescriptor> {
    Ok(OciDescriptor {
        media_type: media_type.to_string(),
        digest: OciDigest::parse(digest).map_err(|error| anyhow::anyhow!("{error}"))?,
        size,
        annotations: BTreeMap::new(),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn ghcr_bearer(
    client: &reqwest::blocking::Client,
    github_token: &str,
    identity: &GithubIdentity,
) -> Result<String> {
    let scope = format!("repository:{}:push,pull", ghcr_repository(identity));
    let response = client
        .get("https://ghcr.io/token")
        .query(&[("service", "ghcr.io"), ("scope", scope.as_str())])
        .bearer_auth(github_token)
        .send()
        .context("request GHCR registry token")?;
    if response.status().is_success()
        && let Ok(body) = response.json::<GhcrTokenResponse>()
        && let Some(token) = body.token.or(body.access_token)
        && !token.is_empty()
    {
        return Ok(token);
    }
    Ok(github_token.to_string())
}

#[derive(Debug, serde::Deserialize)]
struct GhcrTokenResponse {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
}

fn upload_blob(
    client: &reqwest::blocking::Client,
    token: &str,
    identity: &GithubIdentity,
    blob: &Blob,
) -> Result<()> {
    let head = client
        .head(ghcr_blob_url(identity, &blob.digest))
        .bearer_auth(token)
        .send()?;
    if head.status().is_success() {
        return Ok(());
    }
    let start = client
        .post(ghcr_uploads_url(identity))
        .bearer_auth(token)
        .send()
        .with_context(|| format!("start GHCR blob upload {}", blob.digest))?;
    if !start.status().is_success() && start.status().as_u16() != 202 {
        bail!(
            "GHCR blob upload session for {} returned {}",
            blob.digest,
            start.status()
        );
    }
    let location = start
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .context("GHCR upload session missing Location")?;
    let put_url = with_digest_query(&location, &blob.digest);
    let put = client
        .put(&put_url)
        .bearer_auth(token)
        .header("Content-Type", blob.media_type)
        .body(blob.bytes.clone())
        .send()
        .with_context(|| format!("upload GHCR blob {}", blob.digest))?;
    if put.status().is_success() || put.status().as_u16() == 201 {
        return Ok(());
    }
    bail!(
        "GHCR blob upload {} returned {}",
        blob.digest,
        put.status()
    )
}

fn put_manifest(
    client: &reqwest::blocking::Client,
    token: &str,
    identity: &GithubIdentity,
    tag: &str,
    bytes: &[u8],
) -> Result<()> {
    let response = client
        .put(ghcr_manifest_url(identity, tag))
        .bearer_auth(token)
        .header("Content-Type", OCI_IMAGE_MANIFEST_MEDIA_TYPE)
        .body(bytes.to_vec())
        .send()
        .with_context(|| format!("put GHCR manifest {}", ghcr_reference(identity, tag)))?;
    if response.status().is_success() || response.status().as_u16() == 201 {
        return Ok(());
    }
    bail!(
        "GHCR manifest put {} returned {}",
        ghcr_reference(identity, tag),
        response.status()
    )
}

fn with_digest_query(location: &str, digest: &str) -> String {
    let separator = if location.contains('?') { '&' } else { '?' };
    format!("{location}{separator}digest={digest}")
}

/// Pull the packed layer bytes from a GHCR OCI manifest. Used by source
/// fallback when the registry and GitHub Release are both unreachable.
pub fn download_ghcr_layer(
    client: &reqwest::blocking::Client,
    token: Option<&str>,
    url: &str,
    dest: &std::path::Path,
    declared_size: u64,
    max_bytes: u64,
) -> Result<()> {
    if url.contains("/blobs/") {
        return download_blob(client, token, url, dest, declared_size, max_bytes);
    }
    let mut request = client
        .get(url)
        .header("Accept", OCI_IMAGE_MANIFEST_MEDIA_TYPE);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        bail!("{url} returned {}", response.status());
    }
    let manifest: serde_json::Value = response.json()?;
    let layer = manifest
        .get("layers")
        .and_then(|layers| layers.as_array())
        .and_then(|layers| layers.first())
        .context("GHCR manifest has no layers")?;
    let digest = layer
        .get("digest")
        .and_then(|value| value.as_str())
        .context("GHCR layer missing digest")?;
    let blob_url = url
        .split("/manifests/")
        .next()
        .map(|prefix| format!("{prefix}/blobs/{digest}"))
        .context("GHCR manifest URL is not a registry manifest")?;
    download_blob(client, token, &blob_url, dest, declared_size, max_bytes)
}

fn download_blob(
    client: &reqwest::blocking::Client,
    token: Option<&str>,
    url: &str,
    dest: &std::path::Path,
    declared_size: u64,
    max_bytes: u64,
) -> Result<()> {
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        bail!("{url} returned {}", response.status());
    }
    fs::create_dir_all(dest.parent().context("dest has parent")?)?;
    let limit = if declared_size > 0 {
        declared_size.saturating_add(1024 * 1024).min(max_bytes)
    } else {
        max_bytes
    };
    let mut file = fs::File::create(dest)?;
    let mut limited = std::io::Read::take(response, limit.saturating_add(1));
    let copied = std::io::copy(&mut limited, &mut file)?;
    if copied > limit {
        let _ = fs::remove_file(dest);
        bail!("GHCR blob exceeded {limit} bytes from {url}");
    }
    if copied == 0 {
        let _ = fs::remove_file(dest);
        bail!("GHCR blob from {url} was empty");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::source::GithubIdentity;

    #[test]
    fn upload_session_appends_digest_query() {
        assert_eq!(
            with_digest_query(
                "https://ghcr.io/v2/acme/http-kit/blobs/uploads/abc",
                "sha256:dead"
            ),
            "https://ghcr.io/v2/acme/http-kit/blobs/uploads/abc?digest=sha256:dead"
        );
        assert_eq!(
            with_digest_query(
                "https://ghcr.io/v2/acme/http-kit/blobs/uploads/abc?_state=1",
                "sha256:dead"
            ),
            "https://ghcr.io/v2/acme/http-kit/blobs/uploads/abc?_state=1&digest=sha256:dead"
        );
    }

    #[test]
    fn org_packages_url_uses_container_package_name() {
        let identity = GithubIdentity {
            owner: "cliptown".into(),
            repo: "cliptown-cli".into(),
        };
        assert_eq!(
            github_packages_web_url(&identity),
            "https://github.com/orgs/cliptown/packages/container/cliptown-cli"
        );
        assert_eq!(
            ghcr_reference(&identity, "v0.1.0"),
            "ghcr.io/cliptown/cliptown-cli:v0.1.0"
        );
    }
}
