//! Retry GitHub and public R2 when the configured HTTP registry is unreachable.
//!
//! Loopback `file://` and `http://127.0.0.1` registries stay hermetic: tests
//! and air-gapped mirrors never leak to github.com. Production hosts such as
//! `registry.zpkg.net` fall back to guessed public R2 keys and GitHub Release
//! assets, then to a tagged source archive only when no packed digest is known.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::Read as _;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::manifest::{Manifest, is_slug};
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::registry::{
    AuditLogResponse, ClaimOrgResponse, PackageMetadata, PublishMeta, PublishResponse,
    SearchResponse, VersionMetadata, YankResponse,
};
use zed_interfaces::source::{
    ArtifactQuery, ArtifactSourceKind, ArtifactsSection, GithubIdentity, artifact_locators,
    github_api_release_url, github_api_repo_url, github_api_tags_url, github_identity_for,
    github_raw_manifest_url, github_release_sidecar_names, version_from_git_tag,
};
use zed_interfaces::vcs::Vcs;

use crate::registry::{HttpRegistry, Registry};

const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;

thread_local! {
    static CLI_OVERRIDES: RefCell<Option<CliFallbackOverrides>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct CliFallbackOverrides {
    r2_public_base: Option<String>,
    r2_public_key: Option<String>,
    enabled: bool,
}

/// Apply clap `--r2-public-*` / `--source-fallback` without writing process env
/// (edition 2024 `set_var` is unsafe). `from_env` overlays these on env values.
pub fn apply_cli_overrides(
    r2_public_base: Option<String>,
    r2_public_key: Option<String>,
    enabled: bool,
) {
    CLI_OVERRIDES.with(|slot| {
        *slot.borrow_mut() = Some(CliFallbackOverrides {
            r2_public_base,
            r2_public_key,
            enabled,
        });
    });
}

fn max_artifact_bytes() -> u64 {
    std::env::var("ZED_PKG_MAX_ARTIFACT_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_ARTIFACT_BYTES)
}

#[derive(Debug, Clone)]
pub struct SourceFallbackConfig {
    pub enabled: bool,
    pub r2_public_base: Option<String>,
    pub r2_public_key: Option<String>,
    pub github_token: Option<String>,
    pub allow_loopback: bool,
}

impl SourceFallbackConfig {
    pub fn from_env() -> Self {
        let mut config = Self {
            enabled: env_bool("ZED_PKG_SOURCE_FALLBACK", true),
            r2_public_base: env_nonempty("ZED_PKG_R2_PUBLIC_BASE"),
            r2_public_key: env_nonempty("ZED_PKG_R2_PUBLIC_KEY"),
            github_token: env_nonempty("ZED_PKG_GITHUB_TOKEN")
                .or_else(|| env_nonempty("GITHUB_TOKEN"))
                .or_else(|| env_nonempty("GH_TOKEN")),
            // Test-org / local canaries bind the registry to 127.0.0.1 so they
            // can take it down. Production loopback stays hermetic.
            allow_loopback: env_bool("ZED_PKG_SOURCE_FALLBACK_ALLOW_LOOPBACK", false),
        };
        CLI_OVERRIDES.with(|slot| {
            if let Some(over) = slot.borrow().as_ref() {
                if over.r2_public_base.is_some() {
                    config.r2_public_base = over.r2_public_base.clone();
                }
                if over.r2_public_key.is_some() {
                    config.r2_public_key = over.r2_public_key.clone();
                }
                config.enabled = over.enabled;
            }
        });
        config
    }
}

pub struct FallbackRegistry {
    inner: HttpRegistry,
    config: SourceFallbackConfig,
    client: reqwest::blocking::Client,
    cache: Mutex<HashMap<String, VersionMetadata>>,
}

impl FallbackRegistry {
    pub fn wrap(inner: HttpRegistry, registry_url: &str) -> Box<dyn Registry> {
        let config = SourceFallbackConfig::from_env();
        if !config.enabled {
            return Box::new(inner);
        }
        if is_loopback_registry(registry_url) && !config.allow_loopback {
            return Box::new(inner);
        }
        match reqwest::blocking::Client::builder()
            .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(client) => Box::new(Self {
                inner,
                config,
                client,
                cache: Mutex::new(HashMap::new()),
            }),
            Err(_) => Box::new(inner),
        }
    }

    fn github_headers(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        let request = request.header("Accept", "application/vnd.github+json");
        match &self.config.github_token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    fn cached(&self, org: &str, name: &str, version: &str) -> Option<VersionMetadata> {
        self.cache
            .lock()
            .ok()
            .and_then(|guard| guard.get(&cache_key(org, name, version)).cloned())
    }

    fn remember(&self, metadata: VersionMetadata) {
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(
                cache_key(&metadata.org, &metadata.name, &metadata.version),
                metadata,
            );
        }
    }

    fn github_get_package(&self, org: &str, name: &str) -> Result<PackageMetadata> {
        let identity = GithubIdentity::guessed_from_package(org, name);
        let repo: GithubRepo = self
            .github_headers(self.client.get(github_api_repo_url(&identity)))
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .and_then(|response| response.json())
            .unwrap_or(GithubRepo {
                default_branch: "main".to_string(),
                html_url: identity.web_url(),
            });
        let manifest = self
            .fetch_manifest(&identity, &repo.default_branch)
            .or_else(|_| self.fetch_manifest(&identity, "main"))
            .or_else(|_| self.fetch_manifest(&identity, "master"));
        let tags = self.github_tags(&identity)?;
        let versions = versions_from_tags(&tags);
        let (description, vcs, repo_url, version_scheme, keywords) = match manifest {
            Ok(manifest) => (
                manifest.package.description.clone(),
                manifest.package.repository.vcs,
                manifest.package.repository.url.clone(),
                manifest.package.version_scheme,
                manifest.package.keywords.clone(),
            ),
            Err(_) => (
                None,
                Vcs::Git,
                repo.html_url.clone(),
                Default::default(),
                Vec::new(),
            ),
        };
        Ok(PackageMetadata {
            org: org.to_string(),
            name: name.to_string(),
            description,
            vcs,
            repo_url,
            version_scheme,
            latest: versions.first().cloned(),
            tags: keywords,
            versions,
            mirrors: Vec::new(),
            signing_keys: Vec::new(),
        })
    }

    fn github_get_version(&self, org: &str, name: &str, version: &str) -> Result<VersionMetadata> {
        if let Some(cached) = self.cached(org, name, version) {
            return Ok(cached);
        }
        let identity = github_identity_for(org, name, None);
        let tag = format!("v{version}");
        if let Some(metadata) = self.release_sidecar(&identity, org, name, version, &tag) {
            self.remember(metadata.clone());
            return Ok(metadata);
        }
        if let Some(metadata) = self.ghcr_version(&identity, org, name, version, &tag) {
            self.remember(metadata.clone());
            return Ok(metadata);
        }
        // GitHub may be down too. Guess CDN keys from org/name so a public
        // R2 origin can still serve the packed tarball.
        let fetched = self
            .fetch_manifest(&identity, &tag)
            .or_else(|_| self.fetch_manifest(&identity, version));
        let empty = ArtifactsSection::EMPTY;
        let fallback_repo = identity.web_url();
        let (artifacts, repo_url) = match &fetched {
            Ok(manifest) => (
                &manifest.package.artifacts,
                manifest.package.repository.url.as_str(),
            ),
            Err(_) => (&empty, fallback_repo.as_str()),
        };
        let query = ArtifactQuery {
            org,
            name,
            version,
            vcs_tag: &tag,
            sha256: None,
            format: ArtifactFormat::TarGz,
            repo_url: Some(repo_url),
            artifacts: Some(artifacts),
            registry_base: None,
            r2_public_base: self.config.r2_public_base.as_deref(),
            r2_public_key: self.config.r2_public_key.as_deref(),
        };
        let locators = artifact_locators(&query);
        let download_url = locators
            .iter()
            .find(|locator| locator.kind != ArtifactSourceKind::GithubArchive)
            .or_else(|| locators.last())
            .map(|locator| locator.url.clone())
            .context("no fallback locator")?;
        let mut metadata = VersionMetadata {
            org: org.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            sha256: String::new(),
            size: 0,
            format: ArtifactFormat::TarGz,
            vcs_tag: tag,
            vcs_commit: None,
            download_url,
            published_at: "1970-01-01T00:00:00Z".to_string(),
            yanked: false,
            mirrors: locators
                .iter()
                .map(zed_interfaces::mirror::MirrorDescriptorV1::from_locator)
                .collect(),
            signatures: Vec::new(),
        };
        self.fill_digest(&mut metadata)?;
        self.remember(metadata.clone());
        Ok(metadata)
    }

    fn fill_digest(&self, metadata: &mut VersionMetadata) -> Result<()> {
        if zed_interfaces::manifest::is_sha256_hex(&metadata.sha256) {
            return Ok(());
        }
        let tmp = tempfile::NamedTempFile::new().context("fallback digest tempfile")?;
        self.download_locators(metadata, tmp.path())?;
        let (sha256, size) = sha256_and_size(tmp.path())?;
        metadata.sha256 = sha256;
        metadata.size = size;
        Ok(())
    }

    fn fetch_manifest(&self, identity: &GithubIdentity, git_ref: &str) -> Result<Manifest> {
        let url = github_raw_manifest_url(identity, git_ref);
        let response = self
            .github_headers(self.client.get(&url))
            .send()
            .with_context(|| format!("fetch {MANIFEST_FILE} from {url}"))?;
        if !response.status().is_success() {
            bail!(
                "{MANIFEST_FILE} not found at {}@{git_ref} ({})",
                identity.web_url(),
                response.status()
            );
        }
        let text = response.text()?;
        Manifest::parse(&text).map_err(|error| anyhow!(error))
    }

    fn github_tags(&self, identity: &GithubIdentity) -> Result<Vec<GithubTag>> {
        let response = self
            .github_headers(self.client.get(github_api_tags_url(identity)))
            .send()
            .with_context(|| format!("list tags for {}", identity.web_url()))?;
        if !response.status().is_success() {
            bail!(
                "GitHub tags for {} returned {}",
                identity.web_url(),
                response.status()
            );
        }
        Ok(response.json().unwrap_or_default())
    }

    fn release_sidecar(
        &self,
        identity: &GithubIdentity,
        org: &str,
        name: &str,
        version: &str,
        tag: &str,
    ) -> Option<VersionMetadata> {
        let _ = self
            .github_headers(self.client.get(github_api_release_url(identity, tag)))
            .send();
        for sidecar in github_release_sidecar_names(org, name, version) {
            let url = zed_interfaces::source::github_release_download_url(identity, tag, &sidecar);
            let Ok(response) = self.client.get(&url).send() else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            if let Ok(mut metadata) = response.json::<VersionMetadata>() {
                if metadata.org.is_empty() {
                    metadata.org = org.to_string();
                }
                if metadata.name.is_empty() {
                    metadata.name = name.to_string();
                }
                if metadata.version.is_empty() {
                    metadata.version = version.to_string();
                }
                return Some(metadata);
            }
        }
        None
    }

    fn ghcr_version(
        &self,
        identity: &GithubIdentity,
        org: &str,
        name: &str,
        version: &str,
        tag: &str,
    ) -> Option<VersionMetadata> {
        use zed_interfaces::source::ghcr_manifest_url;
        let url = ghcr_manifest_url(identity, tag);
        let mut request = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.oci.image.manifest.v1+json");
        if let Some(token) = &self.config.github_token {
            match crate::github_packages::ghcr_registry_token(&self.client, token, identity, "pull")
            {
                Ok(bearer) => request = request.bearer_auth(bearer),
                Err(_) => return None,
            }
        }
        let Ok(response) = request.send() else {
            return None;
        };
        if !response.status().is_success() {
            return None;
        }
        let Ok(manifest) = response.json::<serde_json::Value>() else {
            return None;
        };
        let layer = manifest.get("layers")?.as_array()?.first()?;
        let digest = layer.get("digest")?.as_str()?.to_string();
        let size = layer.get("size")?.as_u64().unwrap_or(0);
        let sha256 = digest
            .strip_prefix("sha256:")
            .unwrap_or(digest.as_str())
            .to_string();
        let repo_url = identity.web_url();
        let locators = artifact_locators(&ArtifactQuery {
            org,
            name,
            version,
            vcs_tag: tag,
            sha256: Some(sha256.as_str()),
            format: ArtifactFormat::TarGz,
            repo_url: Some(repo_url.as_str()),
            artifacts: Some(&ArtifactsSection::EMPTY),
            registry_base: None,
            r2_public_base: self.config.r2_public_base.as_deref(),
            r2_public_key: self.config.r2_public_key.as_deref(),
        });
        Some(VersionMetadata {
            org: org.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            sha256,
            size,
            format: ArtifactFormat::TarGz,
            vcs_tag: tag.to_string(),
            vcs_commit: None,
            download_url: url,
            published_at: "1970-01-01T00:00:00Z".to_string(),
            yanked: false,
            mirrors: locators
                .iter()
                .map(zed_interfaces::mirror::MirrorDescriptorV1::from_locator)
                .collect(),
            signatures: Vec::new(),
        })
    }

    fn download_locators(&self, version: &VersionMetadata, dest: &Path) -> Result<()> {
        let mut errors = Vec::new();
        let packed_digest = zed_interfaces::manifest::is_sha256_hex(&version.sha256);
        // `github_packages_enabled` is false unless a GitHub repo URL is
        // supplied (or the section opts in). Guess the coordinate as
        // github.com/{org}/{name} so GHCR locators are actually emitted.
        let guessed_repo = GithubIdentity::guessed_from_package(&version.org, &version.name).web_url();
        let locators = artifact_locators(&ArtifactQuery {
            org: &version.org,
            name: &version.name,
            version: &version.version,
            vcs_tag: &version.vcs_tag,
            sha256: packed_digest.then_some(version.sha256.as_str()),
            format: version.format,
            repo_url: Some(guessed_repo.as_str()),
            artifacts: Some(&ArtifactsSection::EMPTY),
            registry_base: None,
            r2_public_base: self.config.r2_public_base.as_deref(),
            r2_public_key: self.config.r2_public_key.as_deref(),
        });
        for locator in locators {
            if locator.kind == ArtifactSourceKind::GithubArchive && packed_digest {
                continue;
            }
            if locator.kind == ArtifactSourceKind::Registry {
                continue;
            }
            let result = if locator.kind == ArtifactSourceKind::GithubPackages {
                crate::github_packages::download_ghcr_layer(
                    &self.client,
                    self.config.github_token.as_deref(),
                    &locator.url,
                    dest,
                    version.size,
                    max_artifact_bytes(),
                )
            } else {
                download_url(&self.client, &locator.url, dest, version.size)
            };
            match result {
                Ok(()) => return Ok(()),
                Err(error) => errors.push(format!("{}: {error}", locator.url)),
            }
        }
        bail!(
            "no GitHub/R2/GHCR fallback succeeded for {}/{}@{} ({})",
            version.org,
            version.name,
            version.version,
            errors.join("; ")
        )
    }
}

impl Registry for FallbackRegistry {
    fn get_package(&self, org: &str, name: &str) -> Result<PackageMetadata> {
        match self.inner.get_package(org, name) {
            Ok(package) => Ok(package),
            Err(error) => match self.github_get_package(org, name) {
                Ok(package) => {
                    eprintln!(
                        "warning: registry unavailable for {org}/{name}; using GitHub fallback ({error})"
                    );
                    Ok(package)
                }
                Err(_) => Err(error),
            },
        }
    }

    fn get_version(&self, org: &str, name: &str, version: &str) -> Result<VersionMetadata> {
        match self.inner.get_version(org, name, version) {
            Ok(metadata) => Ok(metadata),
            Err(error) => match self.github_get_version(org, name, version) {
                Ok(metadata) => {
                    eprintln!(
                        "warning: registry unavailable for {org}/{name}@{version}; using GitHub/R2 fallback ({error})"
                    );
                    Ok(metadata)
                }
                Err(_) => Err(error),
            },
        }
    }

    fn download(&self, version: &VersionMetadata, dest: &Path) -> Result<()> {
        match self.inner.download(version, dest) {
            Ok(()) => Ok(()),
            Err(error) => match self.download_locators(version, dest) {
                Ok(()) => Ok(()),
                Err(locator_error) => Err(error.context(locator_error)),
            },
        }
    }

    fn publish(
        &self,
        meta: &PublishMeta,
        artifact: &Path,
        token: Option<&str>,
    ) -> Result<PublishResponse> {
        self.inner.publish(meta, artifact, token)
    }

    fn claim_org(&self, slug: &str, token: Option<&str>) -> Result<ClaimOrgResponse> {
        self.inner.claim_org(slug, token)
    }

    fn search(&self, query: &str) -> Result<SearchResponse> {
        self.inner.search(query)
    }

    fn yank(
        &self,
        org: &str,
        name: &str,
        version: &str,
        yanked: bool,
        token: Option<&str>,
    ) -> Result<YankResponse> {
        self.inner.yank(org, name, version, yanked, token)
    }

    fn audit_log(
        &self,
        org: &str,
        limit: Option<u64>,
        token: Option<&str>,
    ) -> Result<AuditLogResponse> {
        self.inner.audit_log(org, limit, token)
    }
}

#[derive(Debug, Deserialize)]
struct GithubRepo {
    #[serde(default = "default_branch")]
    default_branch: String,
    #[serde(default)]
    html_url: String,
}

fn default_branch() -> String {
    "main".to_string()
}

#[derive(Debug, Deserialize)]
struct GithubTag {
    name: String,
}

fn versions_from_tags(tags: &[GithubTag]) -> Vec<String> {
    let mut versions: Vec<String> = tags
        .iter()
        .filter_map(|tag| version_from_git_tag(&tag.name))
        .collect();
    zed_interfaces::version::sort_desc(&mut versions);
    versions
}

fn download_url(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    declared_size: u64,
) -> Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("bad fallback url {url}"))?;
    if parsed.scheme() != "https"
        && parsed.scheme() != "http"
        && !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost"))
    {
        bail!("refusing fallback download over {}", parsed.scheme());
    }
    if parsed.scheme() == "http" {
        let loopback = matches!(parsed.host_str(), Some("localhost"))
            || parsed
                .host_str()
                .and_then(|host| host.parse::<std::net::IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback());
        if !loopback {
            bail!("refusing plaintext fallback download from {url}");
        }
    }
    let response = client
        .get(parsed)
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        bail!("{url} returned {}", response.status());
    }
    fs::create_dir_all(dest.parent().context("dest has parent")?)?;
    let cap = max_artifact_bytes();
    let limit = if declared_size > 0 {
        declared_size.saturating_add(1024 * 1024).min(cap)
    } else {
        cap
    };
    let mut file = fs::File::create(dest)?;
    let mut limited = response.take(limit.saturating_add(1));
    let copied = std::io::copy(&mut limited, &mut file)?;
    if copied > limit {
        let _ = fs::remove_file(dest);
        bail!("fallback artifact exceeded {limit} bytes from {url}");
    }
    if copied == 0 {
        let _ = fs::remove_file(dest);
        bail!("fallback artifact from {url} was empty");
    }
    Ok(())
}

fn sha256_and_size(path: &Path) -> Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        size += read as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn cache_key(org: &str, name: &str, version: &str) -> String {
    format!("{org}/{name}@{version}")
}

pub fn is_loopback_registry(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}

/// True when `org`/`name` can be used as a GitHub guess (`github.com/{org}/{name}`).
pub fn github_guess_is_safe(org: &str, name: &str) -> bool {
    is_slug(org) && is_slug(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::source::{r2_object_keys, resolve_r2_public_base};

    #[test]
    fn loopback_registries_are_hermetic() {
        assert!(is_loopback_registry("http://127.0.0.1:18080"));
        assert!(is_loopback_registry("http://localhost:8080"));
        assert!(!is_loopback_registry("https://registry.zpkg.net"));
    }

    #[test]
    fn tag_names_become_published_versions() {
        let tags = vec![
            GithubTag {
                name: "v1.2.0".into(),
            },
            GithubTag {
                name: "v1.0.0".into(),
            },
            GithubTag {
                name: "0.9.0".into(),
            },
        ];
        assert_eq!(
            versions_from_tags(&tags),
            vec![
                "1.2.0".to_string(),
                "1.0.0".to_string(),
                "0.9.0".to_string()
            ]
        );
    }

    #[test]
    fn r2_public_key_env_is_the_guessable_origin() {
        assert_eq!(
            resolve_r2_public_base(None, None, Some("pub-zed")),
            "https://pub-zed.r2.dev"
        );
        let query = ArtifactQuery {
            org: "zed-pkg",
            name: "zed-cli",
            version: "0.1.0",
            vcs_tag: "v0.1.0",
            sha256: None,
            format: ArtifactFormat::TarGz,
            repo_url: Some("https://github.com/zed-pkg/zed-cli"),
            artifacts: None,
            registry_base: None,
            r2_public_base: None,
            r2_public_key: None,
        };
        assert!(
            r2_object_keys(&query)
                .iter()
                .any(|key| key == "github/zed-pkg/zed-cli/v0.1.0/zed-cli-0.1.0.tar.gz")
        );
    }
}
