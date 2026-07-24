use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use semver::{Version, VersionReq};
use zed_interfaces::registry::{
    self, ClaimOrgRequest, ClaimOrgResponse, PackageMetadata, PublishMeta, PublishResponse,
    SearchResponse, VersionMetadata,
};

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
}

pub fn registry_for(url: &str) -> Result<Box<dyn Registry>> {
    if let Some(path) = url.strip_prefix("file://") {
        Ok(Box::new(FileRegistry::new(PathBuf::from(path))))
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
            download_url: format!("file://{}", dest.display()),
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
            versions: Vec::new(),
        });
        if !pkg.versions.contains(version) {
            pkg.versions.push(version.clone());
        }
        zed_interfaces::version::sort_desc(&mut pkg.versions);
        pkg.version_scheme = meta.manifest.package.version_scheme;
        pkg.latest = pkg.versions.first().cloned();
        pkg.description = meta.manifest.package.description.clone();
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
        let url = if version.download_url.starts_with("http") {
            version.download_url.clone()
        } else {
            self.url(&registry::artifact_path(&version.sha256))
        };
        let mut response = Self::check(self.client.get(url).send()?)?;
        fs::create_dir_all(dest.parent().context("dest has parent")?)?;
        let mut file = fs::File::create(dest)?;
        response.copy_to(&mut file)?;
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
}
