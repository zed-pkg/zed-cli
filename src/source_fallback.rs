//! Retry GitHub and public R2 when the configured HTTP registry is unreachable.
//!
//! Loopback `file://` and `http://127.0.0.1` registries stay hermetic: tests
//! and air-gapped mirrors never leak to github.com. Production hosts such as
//! `registry.zpkg.net` fall back to guessed public R2 keys and GitHub Release
//! assets, then to the GitHub tag archive, repacked exactly as `zed publish`
//! would pack that tag.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::Read as _;
use std::path::{Component, Path};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use serde::Deserialize;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::manifest::{Manifest, is_slug};
use zed_interfaces::mirror::MirrorDescriptorV1;
use zed_interfaces::paths::MANIFEST_FILE;
use zed_interfaces::registry::{
    AuditLogResponse, ClaimOrgResponse, PackageMetadata, PublishMeta, PublishResponse,
    SearchResponse, VersionMetadata, YankResponse,
};
use zed_interfaces::source::{
    ArtifactLocator, ArtifactQuery, ArtifactSourceKind, ArtifactsSection, GithubIdentity,
    artifact_locators, github_api_release_url, github_api_repo_url, github_api_tags_url,
    github_raw_manifest_url, github_release_asset_names, github_release_sidecar_names,
    parse_github_identity, resolve_r2_public_base, version_from_git_tag,
};
use zed_interfaces::vcs::Vcs;

use crate::registry::{HttpRegistry, Registry};

const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_GITHUB_REPO_CANDIDATES: usize = 12;
const GITHUB_REPOSITORY_SEARCH_URL: &str = "https://api.github.com/search/repositories";

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
    github_identities: Mutex<HashMap<String, GithubIdentity>>,
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
                github_identities: Mutex::new(HashMap::new()),
            }),
            Err(_) => Box::new(inner),
        }
    }

    fn r2_base(&self) -> String {
        resolve_r2_public_base(
            None,
            self.config.r2_public_base.as_deref(),
            self.config.r2_public_key.as_deref(),
        )
    }

    /// Fetch a GitHub tag archive and repack it as a Zed artifact. The raw
    /// archive's size is unrelated to the packed artifact's, so only the
    /// global cap bounds the download; the caller verifies the packed digest.
    fn download_tag_archive(
        &self,
        identity: &GithubIdentity,
        url: &str,
        version: &VersionMetadata,
        dest: &Path,
    ) -> Result<()> {
        let raw = tempfile::NamedTempFile::new().context("tag archive tempfile")?;
        match self.config.github_token.as_deref() {
            // github.com/<owner>/<repo>/archive/... is anonymous-only, so a
            // private repository 404s there even with a token. The REST
            // tarball endpoint honours the token and redirects to a signed
            // codeload URL for the same tag.
            Some(token) => {
                let api_url = github_api_tarball_url(identity, &version.vcs_tag);
                download_url(&self.client, &api_url, raw.path(), 0, Some(token))?;
            }
            None => download_url(&self.client, url, raw.path(), 0, None)?,
        }
        repack_tag_archive(
            raw.path(),
            &version.org,
            &version.name,
            &version.version,
            dest,
        )
    }

    /// Record the commit a version's tag points at. Fallback metadata otherwise
    /// carries no commit, and a frozen restore compares VCS provenance against
    /// the lock exactly, so it would refuse a digest-identical artifact. The
    /// commit comes from GitHub, never from the archive; if GitHub cannot say,
    /// the field stays empty and that comparison fails closed as before.
    fn fill_tag_commit(&self, identity: &GithubIdentity, metadata: &mut VersionMetadata) {
        if metadata.vcs_commit.is_some() {
            return;
        }
        let url = format!(
            "{}/commits/{}",
            github_api_repo_url(identity),
            metadata.vcs_tag
        );
        metadata.vcs_commit = self
            .github_headers(self.client.get(url))
            .send()
            .ok()
            .filter(|response| response.status().is_success())
            .and_then(|response| response.json::<GithubCommit>().ok())
            .and_then(|commit| commit_sha(&commit.sha));
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

    fn cached_github_identity(&self, org: &str, name: &str) -> Option<GithubIdentity> {
        self.github_identities
            .lock()
            .ok()
            .and_then(|guard| guard.get(&package_key(org, name)).cloned())
    }

    fn remember_github_identity(&self, org: &str, name: &str, identity: &GithubIdentity) {
        if let Ok(mut guard) = self.github_identities.lock() {
            guard.insert(package_key(org, name), identity.clone());
        }
    }

    fn github_repo(&self, identity: &GithubIdentity) -> Result<GithubRepo> {
        let response = self
            .github_headers(self.client.get(github_api_repo_url(identity)))
            .send()
            .with_context(|| format!("fetch GitHub repository {}", identity.web_url()))?;
        if !response.status().is_success() {
            bail!(
                "GitHub repository {} returned {}",
                identity.web_url(),
                response.status()
            );
        }
        response
            .json()
            .with_context(|| format!("decode GitHub repository {}", identity.web_url()))
    }

    fn github_manifest(&self, identity: &GithubIdentity, default_branch: &str) -> Result<Manifest> {
        // Admission is bound to GitHub's reported default branch. A stale
        // `main`/`master` branch must never be allowed to self-claim a package
        // when GitHub reports another branch as authoritative.
        self.fetch_manifest(identity, default_branch)
    }

    fn resolve_github_identity(&self, org: &str, name: &str) -> Result<GithubIdentity> {
        if !github_guess_is_safe(org, name) {
            bail!("refusing unsafe GitHub fallback identity `{org}/{name}`");
        }
        if let Some(identity) = self.cached_github_identity(org, name) {
            return Ok(identity);
        }

        let guessed = GithubIdentity::guessed_from_package(org, name);
        let guessed_failure = match self.github_repo(&guessed) {
            Ok(repo) => match self.github_manifest(&guessed, &repo.default_branch) {
                Ok(manifest)
                    if manifest_self_claims_github_identity(&manifest, org, name, &guessed) =>
                {
                    self.remember_github_identity(org, name, &guessed);
                    return Ok(guessed);
                }
                Ok(_) => format!(
                    "{} exists but its committed {MANIFEST_FILE} does not self-claim package {org}/{name}",
                    guessed.web_url()
                ),
                Err(error) => format!(
                    "{} exists but has no admissible {MANIFEST_FILE}: {error:#}",
                    guessed.web_url()
                ),
            },
            Err(error) => format!("{} was unavailable: {error:#}", guessed.web_url()),
        };

        let discovered = self
            .search_github_identity(org, name)
            .with_context(|| guessed_failure)?;
        self.remember_github_identity(org, name, &discovered);
        Ok(discovered)
    }

    fn search_github_identity(&self, org: &str, name: &str) -> Result<GithubIdentity> {
        let query = github_repository_search_query(org, name);
        let response = self
            .github_headers(
                self.client
                    .get(GITHUB_REPOSITORY_SEARCH_URL)
                    .query(&[("q", query.as_str()), ("per_page", "20")]),
            )
            .send()
            .with_context(|| format!("search GitHub repositories for {org}/{name}"))?;
        if !response.status().is_success() {
            bail!(
                "GitHub repository search for {org}/{name} returned {}",
                response.status()
            );
        }
        let search: GithubRepositorySearch = response
            .json()
            .with_context(|| format!("decode GitHub repository search for {org}/{name}"))?;
        if !github_search_result_is_complete(&search) {
            bail!(
                "GitHub repository search for {org}/{name} returned total_count={} with {} item(s); bounded admission requires a complete result set of at most {} candidates",
                search.total_count,
                search.items.len(),
                MAX_GITHUB_REPO_CANDIDATES
            );
        }

        let mut matches = Vec::new();
        for repo in search.items {
            let Some(candidate) = parse_github_identity(&repo.html_url) else {
                continue;
            };
            if !candidate.owner.eq_ignore_ascii_case(org) {
                continue;
            }
            let Ok(manifest) = self.github_manifest(&candidate, &repo.default_branch) else {
                continue;
            };
            if manifest_self_claims_github_identity(&manifest, org, name, &candidate) {
                matches.push(candidate);
            }
        }
        matches.sort_by(|left, right| {
            left.owner
                .to_ascii_lowercase()
                .cmp(&right.owner.to_ascii_lowercase())
                .then_with(|| {
                    left.repo
                        .to_ascii_lowercase()
                        .cmp(&right.repo.to_ascii_lowercase())
                })
        });
        matches.dedup_by(|left, right| same_github_identity(left, right));

        match matches.as_slice() {
            [identity] => Ok(identity.clone()),
            [] => {
                let hint = if self.config.github_token.is_none() {
                    "; private repositories also require ZED_PKG_GITHUB_TOKEN, GITHUB_TOKEN, or GH_TOKEN"
                } else {
                    ""
                };
                bail!("no manifest-validated GitHub repository was found for {org}/{name}{hint}")
            }
            _ => bail!(
                "multiple GitHub repositories claim package identity {org}/{name}: {}; refusing ambiguous fallback",
                matches
                    .iter()
                    .map(GithubIdentity::web_url)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn github_get_package(&self, org: &str, name: &str) -> Result<PackageMetadata> {
        let identity = self.resolve_github_identity(org, name)?;
        let repo = self.github_repo(&identity)?;
        let manifest = self.github_manifest(&identity, &repo.default_branch);
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
        let identity = self.resolve_github_identity(org, name)?;
        let tag = format!("v{version}");
        if let Some(mut metadata) = self.release_sidecar(&identity, org, name, version, &tag) {
            self.fill_tag_commit(&identity, &mut metadata);
            self.remember(metadata.clone());
            return Ok(metadata);
        }
        if let Some(mut metadata) = self.ghcr_version(&identity, org, name, version, &tag) {
            self.fill_tag_commit(&identity, &mut metadata);
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
            mirrors: fallback_mirrors(&locators, &self.r2_base()),
            signatures: Vec::new(),
        };
        self.fill_tag_commit(&identity, &mut metadata);
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
            let hint = if self.config.github_token.is_none()
                && response.status() == reqwest::StatusCode::NOT_FOUND
            {
                " (repository may be private; private repositories need ZED_PKG_GITHUB_TOKEN, GITHUB_TOKEN, or GH_TOKEN)"
            } else {
                ""
            };
            bail!(
                "GitHub tags for {} returned {}{hint}",
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
        let release = self
            .github_headers(self.client.get(github_api_release_url(identity, tag)))
            .send()
            .ok()
            .and_then(|response| response.error_for_status().ok())
            .and_then(|response| response.json::<GithubRelease>().ok());
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
        release
            .and_then(|release| release_asset_metadata(&release, identity, org, name, version, tag))
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
            request = request.bearer_auth(token);
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
        // Artifact fallback must use the same manifest-validated canonical repository
        // identity as metadata/version fallback. A cold cache is not permission to
        // regress to the conventional org/name guess because valid repositories can
        // carry suffixes such as `.rs`.
        let identity = self.resolve_github_identity(&version.org, &version.name)?;
        let repo_url = identity.web_url();
        let locators = artifact_locators(&ArtifactQuery {
            org: &version.org,
            name: &version.name,
            version: &version.version,
            vcs_tag: &version.vcs_tag,
            sha256: packed_digest.then_some(version.sha256.as_str()),
            format: version.format,
            repo_url: Some(repo_url.as_str()),
            artifacts: Some(&ArtifactsSection::EMPTY),
            registry_base: None,
            r2_public_base: self.config.r2_public_base.as_deref(),
            r2_public_key: self.config.r2_public_key.as_deref(),
        });
        // The GitHub tag archive stays in the chain even when a digest is
        // pinned. It is the last locator and is repacked deterministically, so
        // a digest derived from it is reproducible, and every caller verifies
        // the bytes against the pin. Skipping it broke installs whose digest
        // had been derived from that same archive in another registry instance.
        for locator in locators {
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
            } else if locator.kind == ArtifactSourceKind::GithubArchive {
                self.download_tag_archive(&identity, &locator.url, version, dest)
            } else {
                download_url(&self.client, &locator.url, dest, version.size, None)
            };
            match result {
                Ok(()) => return Ok(()),
                Err(error) => errors.push(format!("{}: {error:#}", locator.url)),
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
                Err(fallback) => Err(error.context(format!(
                    "registry unavailable and the GitHub fallback also failed: {fallback:#}"
                ))),
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
                Err(fallback) => Err(error.context(format!(
                    "registry unavailable and the GitHub/R2 fallback also failed: {fallback:#}"
                ))),
            },
        }
    }

    fn download(&self, version: &VersionMetadata, dest: &Path) -> Result<()> {
        match self.inner.download(version, dest) {
            Ok(()) => Ok(()),
            Err(error) => match self.download_locators(version, dest) {
                Ok(()) => Ok(()),
                Err(fallback) => Err(error.context(format!(
                    "registry download failed and the GitHub/R2 fallback also failed: {fallback:#}"
                ))),
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
struct GithubRepositorySearch {
    total_count: usize,
    items: Vec<GithubRepo>,
}

#[derive(Debug, Deserialize)]
struct GithubRepo {
    default_branch: String,
    #[serde(default)]
    html_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn release_asset_metadata(
    release: &GithubRelease,
    identity: &GithubIdentity,
    org: &str,
    name: &str,
    version: &str,
    tag: &str,
) -> Option<VersionMetadata> {
    for format in [ArtifactFormat::TarGz, ArtifactFormat::Zip] {
        for asset_name in github_release_asset_names(org, name, version, format.extension()) {
            let Some(asset) = release.assets.iter().find(|asset| asset.name == asset_name) else {
                continue;
            };
            let Some(sha256) = asset
                .digest
                .as_deref()
                .and_then(|digest| digest.strip_prefix("sha256:"))
            else {
                continue;
            };
            if !zed_interfaces::manifest::is_sha256_hex(sha256) || asset.size == 0 {
                continue;
            }
            let Ok(url) = reqwest::Url::parse(&asset.browser_download_url) else {
                continue;
            };
            if url.scheme() != "https"
                || !matches!(url.host_str(), Some(host) if host.eq_ignore_ascii_case("github.com"))
            {
                continue;
            }
            let repo_url = identity.web_url();
            let locators = artifact_locators(&ArtifactQuery {
                org,
                name,
                version,
                vcs_tag: tag,
                sha256: Some(sha256),
                format,
                repo_url: Some(repo_url.as_str()),
                artifacts: Some(&ArtifactsSection::EMPTY),
                registry_base: None,
                r2_public_base: None,
                r2_public_key: None,
            });
            return Some(VersionMetadata {
                org: org.to_string(),
                name: name.to_string(),
                version: version.to_string(),
                sha256: sha256.to_string(),
                size: asset.size,
                format,
                vcs_tag: tag.to_string(),
                vcs_commit: None,
                download_url: asset.browser_download_url.clone(),
                published_at: release
                    .published_at
                    .clone()
                    .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string()),
                yanked: false,
                mirrors: locators
                    .iter()
                    .map(zed_interfaces::mirror::MirrorDescriptorV1::from_locator)
                    .collect(),
                signatures: Vec::new(),
            });
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct GithubTag {
    name: String,
}

#[derive(Debug, Deserialize)]
struct GithubCommit {
    sha: String,
}

/// A full lowercase 40-hex Git commit id, or nothing. Abbreviated or oddly
/// cased ids would never compare equal to a lock's recorded commit anyway.
fn commit_sha(value: &str) -> Option<String> {
    (value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then(|| value.to_owned())
}

/// Ceiling on files taken from one GitHub tag archive (inode-exhaustion guard,
/// matching the store's extraction bound).
const MAX_TAG_ARCHIVE_FILES: usize = 200_000;

/// Rebuild, from a GitHub tag archive, the artifact `zed publish` would have
/// produced for that tag.
///
/// `git archive` output has a `<repo>-<tag>/` root, a `pax_global_header`
/// entry, and gzip bytes GitHub does not promise to keep stable; the store
/// accepts none of that, and a raw polyglot manifest still declares targets
/// the published artifact does not. The tree is extracted and packed with the
/// same [`crate::pack::pack_all`] publish uses, keeping the root package.
/// Packing is deterministic, so the digest names the tree rather than
/// GitHub's compressor, and it equals the published digest for a tag packed
/// by this packer — a pinned lockfile can be satisfied from github.com alone.
fn repack_tag_archive(raw: &Path, org: &str, name: &str, version: &str, dest: &Path) -> Result<()> {
    let staging = tempfile::tempdir().context("tag archive staging directory")?;
    let tree = staging.path().join("tree");
    extract_tag_archive(raw, &tree)?;
    let manifest = crate::config::read_manifest(&tree)
        .context("GitHub tag archive has no valid package manifest")?;
    let declared = &manifest.package;
    if declared.org != org || declared.name != name || declared.version != version {
        bail!(
            "GitHub tag archive declares {}/{}@{}, expected {org}/{name}@{version}",
            declared.org,
            declared.name,
            declared.version
        );
    }
    // The same manifest hardening `zed pack` / `zed publish` apply before
    // packing; without it the derived `.zpkg.toml` — and so the digest —
    // differs from the published artifact. Their preflights guard a local
    // checkout's untracked inputs, which a tag archive cannot contain.
    let manifest =
        crate::pack_inputs::harden_manifest(crate::pack_guard::harden_manifest(manifest));
    let packages = crate::pack::pack_all(&tree, &manifest, Some(&staging.path().join("out")))
        .context("repack GitHub tag archive")?;
    let root = packages
        .into_iter()
        .find(|package| package.manifest.package.name == name)
        .with_context(|| {
            format!("GitHub tag archive for {org}/{name}@{version} publishes no root package")
        })?;
    // Callers hand over a store cache path whose directory may not exist yet
    // (a frozen fetch uses a fresh isolated store); `download_url` creates it
    // for the other locators, so this one must too.
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).context("create repacked artifact directory")?;
    }
    fs::copy(&root.packed.path, dest).context("stage repacked GitHub tag archive")?;
    Ok(())
}

/// Extract a GitHub tag archive's tree, without its `<repo>-<tag>/` root, into
/// `tree`. Links, specials, and escaping paths fail closed and sizes are
/// capped, exactly as in the store's extractor: the bytes come from the network.
fn extract_tag_archive(raw: &Path, tree: &Path) -> Result<()> {
    let budget = max_artifact_bytes();
    let mut total: u64 = 0;
    let mut files = 0usize;
    let file = fs::File::open(raw).context("open GitHub tag archive")?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    for entry in archive.entries().context("read GitHub tag archive")? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let kind = entry.header().entry_type();
        if matches!(
            kind,
            tar::EntryType::Directory | tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
        ) {
            continue;
        }
        if kind != tar::EntryType::Regular {
            bail!(
                "GitHub tag archive entry `{}` has unsupported type {kind:?} \
                 (only files and directories are allowed)",
                path.display()
            );
        }
        let mut components = path.components();
        if !matches!(components.next(), Some(Component::Normal(_))) {
            bail!(
                "GitHub tag archive entry `{}` has no root directory",
                path.display()
            );
        }
        let rel = components.as_path();
        if rel.as_os_str().is_empty()
            || !rel.components().all(|c| matches!(c, Component::Normal(_)))
        {
            bail!(
                "GitHub tag archive entry `{}` escapes the archive root",
                path.display()
            );
        }
        files += 1;
        if files > MAX_TAG_ARCHIVE_FILES {
            bail!("GitHub tag archive has more than {MAX_TAG_ARCHIVE_FILES} files; refusing");
        }
        let size = entry.header().size()?;
        total = total.saturating_add(size);
        if total > budget {
            bail!("GitHub tag archive expands past the {budget}-byte cap; refusing");
        }
        let target = tree.join(rel);
        if target.exists() {
            bail!("GitHub tag archive repeats entry `{}`", path.display());
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = fs::File::create(&target)?;
        let copied = std::io::copy(&mut (&mut entry).take(size), &mut out)?;
        if copied != size {
            bail!("GitHub tag archive entry `{}` is truncated", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if entry.header().mode()? & 0o111 != 0 {
                0o755
            } else {
                0o644
            };
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        }
    }
    if files == 0 {
        bail!("GitHub tag archive contains no files");
    }
    Ok(())
}

/// Mirrors recorded on fallback-derived version metadata. An R2 locator is a
/// complete object URL, but an object-store mirror is a *base* that
/// `artifact_urls` appends keys to, so every R2 locator collapses to the one
/// public base. Registry locators are dropped: the registry already heads the
/// mirror chain.
fn fallback_mirrors(locators: &[ArtifactLocator], r2_base: &str) -> Vec<MirrorDescriptorV1> {
    let mut mirrors: Vec<MirrorDescriptorV1> = Vec::new();
    for locator in locators {
        let mirror = match locator.kind {
            ArtifactSourceKind::Registry => continue,
            ArtifactSourceKind::R2 => {
                MirrorDescriptorV1::object_store(r2_base.trim_end_matches('/'))
            }
            ArtifactSourceKind::GithubRelease
            | ArtifactSourceKind::GithubPackages
            | ArtifactSourceKind::GithubArchive => MirrorDescriptorV1::from_locator(locator),
        };
        if !mirrors
            .iter()
            .any(|seen| seen.kind == mirror.kind && seen.url == mirror.url)
        {
            mirrors.push(mirror);
        }
    }
    mirrors
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
    token: Option<&str>,
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
    // The token is only ever sent to api.github.com (REST tarball for private
    // repositories); reqwest drops Authorization on the cross-host redirect to
    // codeload, so it never reaches R2, GHCR, or an arbitrary mirror.
    let mut request = client.get(parsed.clone());
    if let Some(token) = token
        && matches!(parsed.host_str(), Some("api.github.com"))
    {
        request = request.bearer_auth(token);
    }
    let response = request.send().with_context(|| format!("GET {url}"))?;
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

/// REST tarball for a tag: the only GitHub archive URL that accepts a token.
fn github_api_tarball_url(identity: &GithubIdentity, tag: &str) -> String {
    format!(
        "https://api.github.com/repos/{}/{}/tarball/{tag}",
        identity.owner, identity.repo
    )
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

fn package_key(org: &str, name: &str) -> String {
    format!("{org}/{name}")
}

fn github_repository_search_query(org: &str, name: &str) -> String {
    format!("org:{org} {name} in:name fork:false")
}

fn same_github_identity(left: &GithubIdentity, right: &GithubIdentity) -> bool {
    left.owner.eq_ignore_ascii_case(&right.owner) && left.repo.eq_ignore_ascii_case(&right.repo)
}

fn github_search_result_is_complete(search: &GithubRepositorySearch) -> bool {
    search.total_count <= MAX_GITHUB_REPO_CANDIDATES && search.items.len() == search.total_count
}

fn manifest_github_identity_for_package(
    manifest: &Manifest,
    org: &str,
    name: &str,
) -> Option<GithubIdentity> {
    if manifest.package.org != org || manifest.package.name != name {
        return None;
    }
    parse_github_identity(&manifest.package.repository.url)
}

fn manifest_self_claims_github_identity(
    manifest: &Manifest,
    org: &str,
    name: &str,
    candidate: &GithubIdentity,
) -> bool {
    manifest_github_identity_for_package(manifest, org, name)
        .is_some_and(|declared| same_github_identity(candidate, &declared))
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
    use flate2::Compression;
    use flate2::write::GzEncoder;
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
    fn release_asset_digest_recovers_version_metadata_without_a_sidecar() {
        let identity = GithubIdentity {
            owner: "acme".into(),
            repo: "http-kit".into(),
        };
        let sha256 = "cd".repeat(32);
        let release = GithubRelease {
            published_at: Some("2026-09-01T00:00:00Z".into()),
            assets: vec![GithubReleaseAsset {
                name: "zpkg-acme-http-kit-1.2.0.tar.gz".into(),
                browser_download_url:
                    "https://github.com/acme/http-kit/releases/download/v1.2.0/zpkg-acme-http-kit-1.2.0.tar.gz"
                        .into(),
                size: 42,
                digest: Some(format!("sha256:{sha256}")),
            }],
        };
        let metadata =
            release_asset_metadata(&release, &identity, "acme", "http-kit", "1.2.0", "v1.2.0")
                .unwrap();
        assert_eq!(metadata.sha256, sha256);
        assert_eq!(metadata.size, 42);
        assert_eq!(metadata.format, ArtifactFormat::TarGz);
        assert_eq!(
            metadata.download_url,
            "https://github.com/acme/http-kit/releases/download/v1.2.0/zpkg-acme-http-kit-1.2.0.tar.gz"
        );
        assert_eq!(metadata.published_at, "2026-09-01T00:00:00Z");
    }

    #[test]
    fn private_archives_use_the_authenticated_rest_tarball() {
        let identity = GithubIdentity {
            owner: "acme".into(),
            repo: "http-kit".into(),
        };
        assert_eq!(
            github_api_tarball_url(&identity, "v1.2.0"),
            "https://api.github.com/repos/acme/http-kit/tarball/v1.2.0"
        );
    }

    #[test]
    fn manifest_identity_preserves_repository_suffixes() {
        let manifest = Manifest::parse(
            r#"
[package]
org = "ores-otel"
name = "ores-otel-sidecar"
version = "0.1.0"

[package.repository]
vcs = "git"
url = "https://github.com/ores-otel/ores-otel-sidecar.rs"
"#,
        )
        .unwrap();
        let identity =
            manifest_github_identity_for_package(&manifest, "ores-otel", "ores-otel-sidecar")
                .unwrap();
        assert_eq!(identity.owner, "ores-otel");
        assert_eq!(identity.repo, "ores-otel-sidecar.rs");
        assert!(manifest_self_claims_github_identity(
            &manifest,
            "ores-otel",
            "ores-otel-sidecar",
            &identity,
        ));
        let guessed = GithubIdentity::guessed_from_package("ores-otel", "ores-otel-sidecar");
        assert!(
            !manifest_self_claims_github_identity(
                &manifest,
                "ores-otel",
                "ores-otel-sidecar",
                &guessed,
            ),
            "a conventional org/name repo must not inherit a package claim whose manifest points at a different canonical repository"
        );
        assert!(
            manifest_github_identity_for_package(&manifest, "ores-otel", "different-package")
                .is_none()
        );
    }

    #[test]
    fn repository_search_query_is_org_scoped_and_name_only() {
        assert_eq!(
            github_repository_search_query("ores-otel", "ores-otel-sidecar"),
            "org:ores-otel ores-otel-sidecar in:name fork:false"
        );
        assert!(github_guess_is_safe("ores-otel", "ores-otel-sidecar"));
        assert!(!github_guess_is_safe("ores-otel", "sidecar in:description"));
    }

    #[test]
    fn repository_search_admission_requires_complete_bounded_results() {
        let repo = |suffix: usize| GithubRepo {
            default_branch: "main".into(),
            html_url: format!("https://github.com/acme/http-kit-{suffix}"),
        };
        let complete = GithubRepositorySearch {
            total_count: MAX_GITHUB_REPO_CANDIDATES,
            items: (0..MAX_GITHUB_REPO_CANDIDATES).map(repo).collect(),
        };
        assert!(github_search_result_is_complete(&complete));

        let truncated = GithubRepositorySearch {
            total_count: MAX_GITHUB_REPO_CANDIDATES + 1,
            items: (0..MAX_GITHUB_REPO_CANDIDATES).map(repo).collect(),
        };
        assert!(!github_search_result_is_complete(&truncated));

        let incomplete = GithubRepositorySearch {
            total_count: 2,
            items: vec![repo(0)],
        };
        assert!(!github_search_result_is_complete(&incomplete));
    }

    #[test]
    fn github_repo_decode_requires_reported_default_branch() {
        let missing = serde_json::from_str::<GithubRepo>(
            r#"{"html_url":"https://github.com/acme/http-kit"}"#,
        );
        assert!(missing.is_err());

        let present = serde_json::from_str::<GithubRepo>(
            r#"{"default_branch":"trunk","html_url":"https://github.com/acme/http-kit"}"#,
        )
        .unwrap();
        assert_eq!(present.default_branch, "trunk");
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

    #[test]
    fn pinned_downloads_try_the_tag_archive_last() {
        let sha256 = "ab".repeat(32);
        let query = ArtifactQuery {
            org: "ores-wasm-loaders",
            name: "owls-interfaces",
            version: "0.1.1",
            vcs_tag: "v0.1.1",
            sha256: Some(&sha256),
            format: ArtifactFormat::TarGz,
            repo_url: Some("https://github.com/ores-wasm-loaders/owls-interfaces"),
            artifacts: Some(&ArtifactsSection::EMPTY),
            registry_base: None,
            r2_public_base: Some("https://cdn.zpkg.net"),
            r2_public_key: None,
        };
        let locators = artifact_locators(&query);
        let archive = locators
            .iter()
            .position(|locator| locator.kind == ArtifactSourceKind::GithubArchive)
            .expect("a pinned query still yields the GitHub tag archive");
        assert_eq!(archive, locators.len() - 1);
    }

    const FIXTURE_MANIFEST: &[u8] = br#"[package]
org = "ores-wasm-loaders"
name = "owls-interfaces"
version = "0.1.1"
description = "Release manifests and host contracts for shared WASM loaders"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/ores-wasm-loaders/owls-interfaces"

[targets.repository]
dir = "."
"#;

    /// A `git archive`-shaped tarball of a polyglot repository: pax global
    /// header, `<root>/` directory, a nested file, and an executable, all
    /// with non-zero mtimes.
    fn git_archive_fixture(root: &str, mtime: u64, level: u32) -> tempfile::NamedTempFile {
        let fixture = tempfile::NamedTempFile::new().unwrap();
        let encoder = GzEncoder::new(
            fs::File::create(fixture.path()).unwrap(),
            Compression::new(level),
        );
        let mut builder = tar::Builder::new(encoder);
        let comment = b"52 comment=291406191b55606e34b7980b42112e9c77ab690b\n";
        let mut pax = tar::Header::new_ustar();
        pax.set_entry_type(tar::EntryType::XGlobalHeader);
        pax.set_size(comment.len() as u64);
        builder
            .append_data(&mut pax, "pax_global_header", comment.as_slice())
            .unwrap();
        let mut dir = tar::Header::new_ustar();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_size(0);
        dir.set_mode(0o775);
        dir.set_mtime(mtime);
        builder
            .append_data(&mut dir, format!("{root}/"), std::io::empty())
            .unwrap();
        for (path, mode, body) in [
            ("rust/src/lib.rs", 0o664, b"pub fn owls() {}\n".as_slice()),
            (".zpkg.toml", 0o664, FIXTURE_MANIFEST),
            ("scripts/run.sh", 0o775, b"#!/bin/sh\n".as_slice()),
        ] {
            let mut header = tar::Header::new_ustar();
            header.set_size(body.len() as u64);
            header.set_mode(mode);
            header.set_mtime(mtime);
            builder
                .append_data(&mut header, format!("{root}/{path}"), body)
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        fixture
    }

    fn repack(fixture: &Path, name: &str, dest: &Path) -> Result<()> {
        repack_tag_archive(fixture, "ores-wasm-loaders", name, "0.1.1", dest)
    }

    #[test]
    fn tag_archives_repack_into_the_published_root_artifact() {
        let first = git_archive_fixture("owls-interfaces-0.1.1", 1_757_080_440, 9);
        let second = git_archive_fixture("owls-interfaces-291406191b55", 42, 1);
        let out = tempfile::tempdir().unwrap();
        // `a` lands in a cache directory that does not exist yet, as in a
        // frozen fetch's fresh isolated store.
        let (a, b) = (
            out.path().join("cache/artifacts/a.tar.gz"),
            out.path().join("b.tar.gz"),
        );
        repack(first.path(), "owls-interfaces", &a).unwrap();
        repack(second.path(), "owls-interfaces", &b).unwrap();
        assert_eq!(sha256_and_size(&a).unwrap(), sha256_and_size(&b).unwrap());

        let mut archive = tar::Archive::new(GzDecoder::new(fs::File::open(&a).unwrap()));
        let mut manifest = None;
        let mut entries = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            assert_eq!(entry.header().mtime().unwrap(), 0, "{path}");
            if path == "pkg/scripts/run.sh" {
                assert_eq!(entry.header().mode().unwrap(), 0o755);
            }
            if path == "pkg/.zpkg.toml" {
                let mut text = String::new();
                entry.read_to_string(&mut text).unwrap();
                manifest = Some(Manifest::parse(&text).unwrap());
            }
            entries.push(path);
        }
        for expected in [
            "pkg/.zpkg.toml",
            "pkg/rust/src/lib.rs",
            "pkg/scripts/run.sh",
        ] {
            assert!(entries.iter().any(|path| path == expected), "{entries:?}");
        }
        // The published root artifact carries a single-target manifest, so a
        // consumer that requests `rust` is not refused over `[targets]`.
        let manifest = manifest.expect("packed manifest");
        assert_eq!(manifest.package.name, "owls-interfaces");
        assert!(manifest.targets.is_empty());
        // `zed pack` hardens the manifest before packing; skipping that changes
        // the derived manifest bytes and so the digest a lockfile pins.
        for hardened in [".zedinclude", "**/.git/**"] {
            assert!(
                manifest.publish.exclude.iter().any(|rule| rule == hardened),
                "{:?}",
                manifest.publish.exclude
            );
        }
    }

    #[test]
    fn tag_commits_must_be_full_lowercase_git_ids() {
        let sha = "291406191b55606e34b7980b42112e9c77ab690b";
        assert_eq!(commit_sha(sha).as_deref(), Some(sha));
        assert_eq!(commit_sha("2914061"), None);
        assert_eq!(commit_sha(&sha.to_ascii_uppercase()), None);
        assert_eq!(commit_sha(&format!("{sha}0")), None);
        assert_eq!(
            commit_sha("../../../etc/passwd-padding-to-forty-chars"),
            None
        );
    }

    #[test]
    fn tag_archive_identity_must_match_the_request() {
        let fixture = git_archive_fixture("other-0.1.1", 1, 6);
        let out = tempfile::tempdir().unwrap();
        let error = repack(fixture.path(), "other", &out.path().join("a.tar.gz"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("expected ores-wasm-loaders/other@0.1.1"),
            "{error}"
        );
    }

    #[test]
    fn tag_archive_links_fail_closed() {
        let fixture = tempfile::NamedTempFile::new().unwrap();
        let mut builder = tar::Builder::new(GzEncoder::new(
            fs::File::create(fixture.path()).unwrap(),
            Compression::default(),
        ));
        let mut link = tar::Header::new_ustar();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        builder
            .append_link(&mut link, "repo-1.0.0/escape", "/etc/passwd")
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let out = tempfile::tempdir().unwrap();
        let error = repack(fixture.path(), "repo", &out.path().join("a.tar.gz"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported type"), "{error}");
    }

    #[test]
    fn fallback_mirrors_use_object_store_bases_not_object_urls() {
        use zed_interfaces::mirror::MirrorKindV1;
        let query = ArtifactQuery {
            org: "ores-wasm-loaders",
            name: "owls-interfaces",
            version: "0.1.1",
            vcs_tag: "v0.1.1",
            sha256: None,
            format: ArtifactFormat::TarGz,
            repo_url: Some("https://github.com/ores-wasm-loaders/owls-interfaces"),
            artifacts: None,
            registry_base: None,
            r2_public_base: Some("https://cdn.zpkg.net"),
            r2_public_key: None,
        };
        let locators = artifact_locators(&query);
        assert!(
            locators
                .iter()
                .filter(|locator| locator.kind == ArtifactSourceKind::R2)
                .count()
                > 1
        );
        let mirrors = fallback_mirrors(&locators, "https://cdn.zpkg.net/");
        let stores: Vec<_> = mirrors
            .iter()
            .filter(|mirror| mirror.kind == MirrorKindV1::ObjectStore)
            .collect();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].url.as_deref(), Some("https://cdn.zpkg.net"));
        assert!(
            mirrors
                .iter()
                .all(|mirror| mirror.kind != MirrorKindV1::ZedRegistry)
        );
    }
}
