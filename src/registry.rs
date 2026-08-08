use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use semver::{Version, VersionReq};
use zed_interfaces::registry::{
    self, AuditLogResponse, ClaimOrgRequest, ClaimOrgResponse, PackageMetadata, PublishMeta,
    PublishResponse, SearchResponse, VersionMetadata, YankRequest, YankResponse,
};

/// Hard ceiling on artifact download size (bytes); the registry-reported
/// size is advisory and attacker-influencable, so a static cap backs it up.
/// Override with `ZED_PKG_MAX_ARTIFACT_BYTES`.
const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;

fn max_artifact_bytes() -> u64 {
    std::env::var("ZED_PKG_MAX_ARTIFACT_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_ARTIFACT_BYTES)
}

fn file_registry_path(raw: &str) -> Result<PathBuf> {
    let url =
        reqwest::Url::parse(raw).with_context(|| format!("invalid file registry url `{raw}`"))?;
    if url.scheme() != "file" {
        bail!("file registry url must use the `file` scheme");
    }
    if let Some(host) = url
        .host_str()
        .filter(|host| !host.is_empty() && !host.eq_ignore_ascii_case("localhost"))
    {
        bail!("unsupported file registry authority `{host}`; only local file URLs are supported");
    }
    url.to_file_path()
        .map_err(|_| anyhow!("file registry url `{raw}` is not a local filesystem path"))
}

fn file_url_for_path(path: &Path) -> Result<String> {
    reqwest::Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| {
            anyhow!(
                "cannot encode local file registry path `{}` as a file URL",
                path.display()
            )
        })
}

/// Client-side registry abstraction. `file://` URLs get a directory-backed
/// registry (hermetic tests, `zed test-local`, air-gapped mirrors); anything
/// else goes over HTTP to a `zed-api-server`.
pub trait Registry {
    fn get_package(&self, org: &str, name: &str) -> Result<PackageMetadata>;
    fn get_version(&self, org: &str, name: &str, version: &str) -> Result<VersionMetadata>;
    fn download(&self, version: &VersionMetadata, dest: &Path) -> Result<()>;
    fn publish(
        &self,
        meta: &PublishMeta,
        artifact: &Path,
        token: Option<&str>,
    ) -> Result<PublishResponse>;
    fn claim_org(&self, slug: &str, token: Option<&str>) -> Result<ClaimOrgResponse>;
    fn search(&self, query: &str) -> Result<SearchResponse>;
    fn yank(
        &self,
        org: &str,
        name: &str,
        version: &str,
        yanked: bool,
        token: Option<&str>,
    ) -> Result<YankResponse>;
    /// The org's audit log, newest first (owner-scoped; zed-docs issue #7).
    fn audit_log(
        &self,
        org: &str,
        limit: Option<u64>,
        token: Option<&str>,
    ) -> Result<AuditLogResponse>;
}

pub fn registry_for(url: &str) -> Result<Box<dyn Registry>> {
    if url.starts_with("file://") {
        Ok(Box::new(FileRegistry::new(file_registry_path(url)?)))
    } else if url.starts_with("http://") || url.starts_with("https://") {
        Ok(Box::new(HttpRegistry::new(url.to_string())?))
    } else {
        bail!("unsupported registry url `{url}` (expected http(s):// or file://)");
    }
}

/// Pick the highest stable version satisfying `req`.
pub fn resolve_version(req: &VersionReq, versions: &[String]) -> Option<Version> {
    versions
        .iter()
        .filter_map(|v| Version::parse(v).ok())
        .filter(|v| req.matches(v))
        .max()
}

// ---------------------------------------------------------------------------
// file:// registry

pub struct FileRegistry {
    root: PathBuf,
}

impl FileRegistry {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn package_json(&self, org: &str, name: &str) -> PathBuf {
        self.root
            .join("packages")
            .join(org)
            .join(name)
            .join("package.json")
    }

    fn version_json(&self, org: &str, name: &str, version: &str) -> PathBuf {
        self.root
            .join("packages")
            .join(org)
            .join(name)
            .join("versions")
            .join(format!("{version}.json"))
    }

    fn artifact_file(&self, sha256: &str) -> PathBuf {
        self.root.join("artifacts").join(format!("{sha256}.tar.gz"))
    }
}

impl Registry for FileRegistry {
    fn get_package(&self, org: &str, name: &str) -> Result<PackageMetadata> {
        let path = self.package_json(org, name);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("package {org}/{name} not found in file registry"))?;
        Ok(serde_json::from_str(&text)?)
    }

    fn get_version(&self, org: &str, name: &str, version: &str) -> Result<VersionMetadata> {
        let path = self.version_json(org, name, version);
        let text = fs::read_to_string(&path).with_context(|| {
            format!("version {org}/{name}@{version} not found in file registry")
        })?;
        Ok(serde_json::from_str(&text)?)
    }

    fn download(&self, version: &VersionMetadata, dest: &Path) -> Result<()> {
        let src = self.artifact_file(&version.sha256);
        fs::create_dir_all(dest.parent().context("dest has parent")?)?;
        fs::copy(&src, dest)
            .with_context(|| format!("artifact {} missing from file registry", src.display()))?;
        Ok(())
    }

    fn publish(
        &self,
        meta: &PublishMeta,
        artifact: &Path,
        _token: Option<&str>,
    ) -> Result<PublishResponse> {
        let org = &meta.manifest.package.org;
        let name = &meta.manifest.package.name;
        let version = &meta.manifest.package.version;

        let dest = self.artifact_file(&meta.sha256);
        fs::create_dir_all(dest.parent().context("artifacts dir")?)?;
        fs::copy(artifact, &dest)?;

        let vm = VersionMetadata {
            org: org.clone(),
            name: name.clone(),
            version: version.clone(),
            sha256: meta.sha256.clone(),
            size: meta.size,
            format: meta.format,
            vcs_tag: meta.vcs_tag.clone(),
            vcs_commit: meta.vcs_commit.clone(),
            download_url: file_url_for_path(&dest)?,
            published_at: "1970-01-01T00:00:00Z".to_string(),
            yanked: false,
        };
        let vpath = self.version_json(org, name, version);
        fs::create_dir_all(vpath.parent().context("versions dir")?)?;
        fs::write(&vpath, serde_json::to_string_pretty(&vm)?)?;

        let mut pkg = self.get_package(org, name).unwrap_or(PackageMetadata {
            org: org.clone(),
            name: name.clone(),
            description: meta.manifest.package.description.clone(),
            vcs: meta.manifest.package.repository.vcs,
            repo_url: meta.manifest.package.repository.url.clone(),
            version_scheme: meta.manifest.package.version_scheme,
            latest: None,
            tags: meta.manifest.package.keywords.clone(),
            versions: Vec::new(),
        });
        if !pkg.versions.contains(version) {
            pkg.versions.push(version.clone());
        }
        zed_interfaces::version::sort_desc(&mut pkg.versions);
        pkg.version_scheme = meta.manifest.package.version_scheme;
        pkg.latest = pkg.versions.first().cloned();
        pkg.description = meta.manifest.package.description.clone();
        // Keywords are refreshed from the newest publish, the same way the
        // description is, so discovery reflects the current manifest.
        pkg.tags = meta.manifest.package.keywords.clone();
        fs::write(
            self.package_json(org, name),
            serde_json::to_string_pretty(&pkg)?,
        )?;

        Ok(PublishResponse {
            org: org.clone(),
            name: name.clone(),
            version: version.clone(),
            sha256: meta.sha256.clone(),
        })
    }

    fn claim_org(&self, slug: &str, _token: Option<&str>) -> Result<ClaimOrgResponse> {
        let dir = self.root.join("packages").join(slug);
        let created = !dir.exists();
        fs::create_dir_all(&dir)?;
        Ok(ClaimOrgResponse {
            slug: slug.to_string(),
            created,
        })
    }

    fn yank(
        &self,
        org: &str,
        name: &str,
        version: &str,
        yanked: bool,
        _token: Option<&str>,
    ) -> Result<YankResponse> {
        let vpath = self.version_json(org, name, version);
        let text = fs::read_to_string(&vpath)
            .with_context(|| format!("version {org}/{name}@{version} not found"))?;
        let mut vm: VersionMetadata = serde_json::from_str(&text)?;
        vm.yanked = yanked;
        fs::write(&vpath, serde_json::to_string_pretty(&vm)?)?;
        Ok(YankResponse {
            org: org.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            yanked,
        })
    }

    /// A `file://` registry is a plain directory with no server enforcing
    /// authority, so there is no trustworthy "who did what" to report. Say so
    /// rather than returning an empty log that could be mistaken for "nothing
    /// ever happened".
    fn audit_log(
        &self,
        _org: &str,
        _limit: Option<u64>,
        _token: Option<&str>,
    ) -> Result<AuditLogResponse> {
        bail!(
            "a file:// registry keeps no audit log (no server records who acted); \
             point --registry at a zed-api-server to read one"
        )
    }

    fn search(&self, query: &str) -> Result<SearchResponse> {
        let mut items = Vec::new();
        let packages_root = self.root.join("packages");
        let query_lower = query.to_lowercase();
        if packages_root.is_dir() {
            for org in fs::read_dir(&packages_root)?.flatten() {
                if !org.path().is_dir() {
                    continue;
                }
                for pkg in fs::read_dir(org.path())?.flatten() {
                    let Ok(meta) = self.get_package(
                        &org.file_name().to_string_lossy(),
                        &pkg.file_name().to_string_lossy(),
                    ) else {
                        continue;
                    };
                    let haystack = format!(
                        "{}/{} {}",
                        meta.org,
                        meta.name,
                        meta.description.as_deref().unwrap_or_default()
                    )
                    .to_lowercase();
                    if haystack.contains(&query_lower) {
                        items.push(zed_interfaces::registry::PackageSummary {
                            org: meta.org,
                            name: meta.name,
                            description: meta.description,
                            latest: meta.latest,
                            tags: meta.tags,
                        });
                    }
                }
            }
        }
        Ok(SearchResponse {
            query: query.to_string(),
            items,
        })
    }
}

// ---------------------------------------------------------------------------
// http registry

pub struct HttpRegistry {
    base: String,
    client: reqwest::blocking::Client,
}

impl HttpRegistry {
    pub fn new(base: String) -> Result<Self> {
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            client: reqwest::blocking::Client::builder()
                .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn check(response: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
        if response.status().is_success() || response.status().is_redirection() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<zed_interfaces::registry::ApiError>(&body) {
            Err(anyhow!(
                "registry error ({status}): {}: {}",
                err.code,
                err.message
            ))
        } else {
            Err(anyhow!("registry error ({status}): {body}"))
        }
    }

    /// The registry hands us a `download_url` (possibly a presigned S3/R2
    /// URL on another host). Trusting it verbatim would let a malicious
    /// registry response redirect fetches to plaintext or internal hosts,
    /// so the scheme is checked: https is always fine; http only for
    /// loopback or when the registry itself was configured over http (the
    /// operator already accepted plaintext for that registry).
    fn allowed_download_url(&self, raw: &str) -> Result<reqwest::Url> {
        let url = reqwest::Url::parse(raw).with_context(|| format!("bad download url {raw}"))?;
        let loopback = matches!(url.host_str(), Some("localhost"))
            || url
                .host_str()
                .and_then(|h| h.parse::<std::net::IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback());
        match url.scheme() {
            "https" => Ok(url),
            "http" if loopback || self.base.starts_with("http://") => Ok(url),
            other => bail!(
                "refusing artifact download over `{other}` from {raw} \
                 (https required for non-local registries)"
            ),
        }
    }

    /// Resolve an artifact URL without defeating an explicit registry
    /// override. A server's ordinary `/v1/artifacts/<sha>` URL is only its
    /// public canonical address, so mirrors, port-forwards, and local
    /// registries must fetch that route from `self.base`. A genuinely
    /// external/presigned object URL (different path and/or signed query) is
    /// still honored.
    fn artifact_download_url(&self, raw: &str, sha256: &str) -> Result<reqwest::Url> {
        let artifact_path = registry::artifact_path(sha256);
        if raw.starts_with("http") {
            let advertised = self.allowed_download_url(raw)?;
            if advertised.path() != artifact_path || advertised.query().is_some() {
                return Ok(advertised);
            }
        }
        reqwest::Url::parse(&self.url(&artifact_path)).context("registry url is valid")
    }
}

impl Registry for HttpRegistry {
    fn get_package(&self, org: &str, name: &str) -> Result<PackageMetadata> {
        let response = self
            .client
            .get(self.url(&registry::package_path(org, name)))
            .send()?;
        Ok(Self::check(response)?.json()?)
    }

    fn get_version(&self, org: &str, name: &str, version: &str) -> Result<VersionMetadata> {
        let response = self
            .client
            .get(self.url(&registry::version_path(org, name, version)))
            .send()?;
        Ok(Self::check(response)?.json()?)
    }

    fn download(&self, version: &VersionMetadata, dest: &Path) -> Result<()> {
        let url = self.artifact_download_url(&version.download_url, &version.sha256)?;
        let response = Self::check(self.client.get(url).send()?)?;
        fs::create_dir_all(dest.parent().context("dest has parent")?)?;
        // Bound what we write to disk: the declared size (when sane) plus
        // slack, backed by the global cap. Hash verification happens later
        // in Store::add_artifact; this guard is about disk exhaustion.
        let cap = max_artifact_bytes();
        let limit = if version.size > 0 {
            version.size.saturating_add(1024 * 1024).min(cap)
        } else {
            cap
        };
        let mut file = fs::File::create(dest)?;
        use std::io::Read as _;
        let mut limited = response.take(limit.saturating_add(1));
        let copied = std::io::copy(&mut limited, &mut file)?;
        if copied > limit {
            let _ = fs::remove_file(dest);
            bail!(
                "artifact exceeded its declared size ({} > {limit} bytes); refusing",
                copied
            );
        }
        Ok(())
    }

    fn publish(
        &self,
        meta: &PublishMeta,
        artifact: &Path,
        token: Option<&str>,
    ) -> Result<PublishResponse> {
        let manifest = &meta.manifest.package;
        let form = reqwest::blocking::multipart::Form::new()
            .text(registry::PUBLISH_META_FIELD, serde_json::to_string(meta)?)
            .file(registry::PUBLISH_ARTIFACT_FIELD, artifact)?;
        let mut request = self
            .client
            .put(self.url(&registry::version_path(
                &manifest.org,
                &manifest.name,
                &manifest.version,
            )))
            .multipart(form);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(Self::check(request.send()?)?.json()?)
    }

    fn claim_org(&self, slug: &str, token: Option<&str>) -> Result<ClaimOrgResponse> {
        let mut request =
            self.client
                .post(self.url(&registry::orgs_path()))
                .json(&ClaimOrgRequest {
                    slug: slug.to_string(),
                });
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(Self::check(request.send()?)?.json()?)
    }

    fn search(&self, query: &str) -> Result<SearchResponse> {
        let response = self
            .client
            .get(self.url(&registry::search_path()))
            .query(&[("q", query)])
            .send()?;
        Ok(Self::check(response)?.json()?)
    }

    fn yank(
        &self,
        org: &str,
        name: &str,
        version: &str,
        yanked: bool,
        token: Option<&str>,
    ) -> Result<YankResponse> {
        let mut request = self
            .client
            .post(self.url(&registry::yank_path(org, name, version)))
            .json(&YankRequest { yanked });
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(Self::check(request.send()?)?.json()?)
    }

    fn audit_log(
        &self,
        org: &str,
        limit: Option<u64>,
        token: Option<&str>,
    ) -> Result<AuditLogResponse> {
        let mut request = self.client.get(self.url(&registry::audit_path(org)));
        if let Some(limit) = limit {
            request = request.query(&[("limit", limit.to_string())]);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(Self::check(request.send()?)?.json()?)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{HttpRegistry, file_registry_path, file_url_for_path};

    #[test]
    fn remote_file_registry_authority_is_rejected() {
        let error = file_registry_path("file://example.test/registry").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported file registry authority")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_file_registry_url_round_trips_and_decodes_spaces() {
        let path = PathBuf::from("/tmp/zed registry");
        let url = file_url_for_path(&path).unwrap();
        assert_eq!(url, "file:///tmp/zed%20registry");
        assert_eq!(file_registry_path(&url).unwrap(), path);
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_registry_url_round_trips_drive_paths_and_spaces() {
        let path = PathBuf::from(r"C:\zed registry");
        let url = file_url_for_path(&path).unwrap();
        assert_eq!(url, "file:///C:/zed%20registry");
        assert_eq!(file_registry_path(&url).unwrap(), path);
    }

    #[test]
    fn canonical_artifact_url_respects_registry_override() {
        let registry = HttpRegistry::new("http://127.0.0.1:18080".into()).unwrap();
        let url = registry
            .artifact_download_url("https://registry.zpkg.tech/v1/artifacts/abc123", "abc123")
            .unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:18080/v1/artifacts/abc123");
    }

    #[test]
    fn presigned_external_artifact_url_is_preserved() {
        let registry = HttpRegistry::new("https://registry.zpkg.tech".into()).unwrap();
        let url = registry
            .artifact_download_url(
                "https://objects.example.test/bucket/abc123?X-Amz-Signature=signed",
                "abc123",
            )
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://objects.example.test/bucket/abc123?X-Amz-Signature=signed"
        );
    }
}
