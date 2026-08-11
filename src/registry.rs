use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use semver::{Version, VersionReq};
use sha2::{Digest, Sha256};
use zed_interfaces::binary_artifact::{
    BINARY_ARTIFACT_METADATA_SCHEMA_V1, BinaryArchiveFormatV1, BinaryArtifactMetadataV1,
    BinaryArtifactPublishMetaV1, BinarySourceProvenanceV1,
};
use zed_interfaces::manifest::is_slug;
use zed_interfaces::registry::{
    self, AuditLogResponse, ClaimOrgRequest, ClaimOrgResponse, PackageMetadata, PublishMeta,
    PublishResponse, SearchResponse, VersionMetadata, YankRequest, YankResponse,
};

/// Hard ceiling on artifact download size (bytes); the registry-reported
/// size is advisory and attacker-influencable, so a static cap backs it up.
/// Override with `ZED_PKG_MAX_ARTIFACT_BYTES`.
const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_REGISTRY_ERROR_BYTES: u64 = 64 * 1024;

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

fn validate_binary_route_identity(
    org: &str,
    name: &str,
    version: &str,
    target: &str,
) -> Result<()> {
    ensure!(
        is_slug(org) && is_slug(name),
        "invalid binary registry package identity `{org}/{name}`"
    );
    ensure!(
        !version.is_empty()
            && version.len() <= 256
            && !matches!(version, "." | "..")
            && !version.bytes().any(|byte| {
                byte.is_ascii_control()
                    || byte.is_ascii_whitespace()
                    || matches!(byte, b'/' | b'\\' | b':' | b'?' | b'#' | b'%')
            }),
        "binary registry version is not a safe route segment"
    );
    ensure!(
        !target.is_empty()
            && target.len() <= 128
            && target.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
            && target
                .as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            && target
                .as_bytes()
                .last()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "binary registry target is not a safe route segment"
    );
    Ok(())
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
    /// Read one target-qualified binary artifact without encoding the target
    /// into release version metadata.
    fn get_binary_artifact(
        &self,
        _org: &str,
        _name: &str,
        _version: &str,
        _target: &str,
        _format: BinaryArchiveFormatV1,
    ) -> Result<BinaryArtifactMetadataV1> {
        bail!("registry does not implement target-qualified binary artifacts")
    }
    fn download_binary_artifact(
        &self,
        _metadata: &BinaryArtifactMetadataV1,
        _dest: &Path,
    ) -> Result<()> {
        bail!("registry does not implement target-qualified binary artifacts")
    }
    /// Publish one target-qualified binary artifact. Legacy publication stays
    /// available through `publish` for one-artifact registries.
    fn publish_binary_artifact(
        &self,
        _meta: &BinaryArtifactPublishMetaV1,
        _artifact: &Path,
        _token: Option<&str>,
    ) -> Result<BinaryArtifactMetadataV1> {
        bail!("registry does not implement target-qualified binary artifacts")
    }
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

    fn binary_artifact_json(
        &self,
        org: &str,
        name: &str,
        version: &str,
        target: &str,
        format: BinaryArchiveFormatV1,
    ) -> PathBuf {
        self.root
            .join("packages")
            .join(org)
            .join(name)
            .join("versions")
            .join(version)
            .join("artifacts")
            .join(target)
            .join(format!("{}.json", format.as_str()))
    }

    fn artifact_file(
        &self,
        sha256: &str,
        format: zed_interfaces::artifact::ArtifactFormat,
    ) -> PathBuf {
        self.root
            .join("artifacts")
            .join(format!("{sha256}.{}", format.extension()))
    }

    fn publish_at(
        &self,
        meta: &PublishMeta,
        artifact: &Path,
        version_path: &Path,
    ) -> Result<PublishResponse> {
        let org = &meta.manifest.package.org;
        let name = &meta.manifest.package.name;
        let version = &meta.manifest.package.version;

        let dest = self.artifact_file(&meta.sha256, meta.format);
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
        fs::create_dir_all(version_path.parent().context("versions dir")?)?;
        fs::write(version_path, serde_json::to_string_pretty(&vm)?)?;

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

    fn qualified_blob_file(&self, sha256: &str) -> PathBuf {
        self.root.join("artifacts").join(format!("{sha256}.zip"))
    }

    fn ensure_qualified_blob(&self, artifact: &Path, sha256: &str, size: u64) -> Result<PathBuf> {
        let destination = self.qualified_blob_file(sha256);
        if immutable_file_matches(&destination, sha256, size)? {
            return Ok(destination);
        }
        let parent = destination.parent().context("qualified artifacts dir")?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        let source_metadata = fs::symlink_metadata(artifact)
            .with_context(|| format!("inspecting binary artifact {}", artifact.display()))?;
        ensure!(
            source_metadata.is_file() && !source_metadata.file_type().is_symlink(),
            "binary artifact must be a regular, non-symlink file"
        );
        let mut source = fs::File::open(artifact)?;
        let mut hasher = Sha256::new();
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            copied = copied
                .checked_add(read as u64)
                .context("binary artifact size overflows u64")?;
            ensure!(
                copied <= size,
                "binary artifact exceeds its declared {size}-byte size"
            );
            hasher.update(&buffer[..read]);
            temporary.write_all(&buffer[..read])?;
        }
        ensure!(
            copied == size && hex::encode(hasher.finalize()) == sha256,
            "binary artifact bytes do not match the declared digest and size"
        );
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        match fs::hard_link(temporary.path(), &destination) {
            Ok(()) => sync_registry_parent(&destination)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    immutable_file_matches(&destination, sha256, size)?,
                    "immutable file registry blob appeared with conflicting bytes"
                );
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "promoting immutable file registry blob {}",
                        destination.display()
                    )
                });
            }
        }
        Ok(destination)
    }

    fn persist_qualified_metadata(
        &self,
        path: &Path,
        metadata: &BinaryArtifactMetadataV1,
    ) -> Result<BinaryArtifactMetadataV1> {
        if let Some(existing) = read_qualified_metadata_if_present(path)? {
            existing.validate()?;
            ensure!(
                immutable_binary_metadata_matches(&existing, metadata),
                "target-qualified binary artifact metadata is immutable and already differs"
            );
            return Ok(existing);
        }
        let parent = path.parent().context("qualified metadata dir")?;
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&serde_json::to_vec_pretty(metadata)?)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        match fs::hard_link(temporary.path(), path) {
            Ok(()) => {
                sync_registry_parent(path)?;
                Ok(metadata.clone())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = read_qualified_metadata_if_present(path)?
                    .context("qualified metadata appeared but cannot be read")?;
                existing.validate()?;
                ensure!(
                    immutable_binary_metadata_matches(&existing, metadata),
                    "target-qualified binary artifact metadata raced with a conflicting publish"
                );
                Ok(existing)
            }
            Err(error) => Err(error)
                .with_context(|| format!("promoting immutable metadata {}", path.display())),
        }
    }
}

fn immutable_binary_metadata_matches(
    existing: &BinaryArtifactMetadataV1,
    proposed: &BinaryArtifactMetadataV1,
) -> bool {
    existing.org == proposed.org
        && existing.name == proposed.name
        && existing.version == proposed.version
        && existing.platform == proposed.platform
        && existing.format == proposed.format
        && existing.sha256 == proposed.sha256
        && existing.size == proposed.size
        && existing.descriptor_sha256 == proposed.descriptor_sha256
        && existing.source == proposed.source
        && existing.attachments == proposed.attachments
}

fn read_qualified_metadata_if_present(path: &Path) -> Result<Option<BinaryArtifactMetadataV1>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "qualified binary metadata path is not a regular, non-symlink file"
    );
    Ok(Some(serde_json::from_slice(&fs::read(path)?)?))
}

fn immutable_file_matches(path: &Path, sha256: &str, size: u64) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "immutable file registry blob is not a regular, non-symlink file"
    );
    ensure!(
        metadata.len() == size,
        "immutable file registry blob has conflicting size"
    );
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .context("immutable blob size overflows u64")?;
        ensure!(
            copied <= size,
            "immutable file registry blob grew while read"
        );
        hasher.update(&buffer[..read]);
    }
    ensure!(
        copied == size && hex::encode(hasher.finalize()) == sha256,
        "immutable file registry blob has conflicting bytes"
    );
    Ok(true)
}

#[cfg(unix)]
fn sync_registry_parent(path: &Path) -> Result<()> {
    fs::File::open(path.parent().context("registry path has no parent")?)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_registry_parent(_path: &Path) -> Result<()> {
    Ok(())
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
        let src = self.artifact_file(&version.sha256, version.format);
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
        let vpath = self.version_json(org, name, version);
        self.publish_at(meta, artifact, &vpath)
    }

    fn get_binary_artifact(
        &self,
        org: &str,
        name: &str,
        version: &str,
        target: &str,
        format: BinaryArchiveFormatV1,
    ) -> Result<BinaryArtifactMetadataV1> {
        validate_binary_route_identity(org, name, version, target)?;
        let path = self.binary_artifact_json(org, name, version, target, format);
        let text = fs::read_to_string(&path).with_context(|| {
            format!(
                "binary artifact {org}/{name}@{version} for {target}/{} not found in file registry",
                format.as_str()
            )
        })?;
        let metadata: BinaryArtifactMetadataV1 = serde_json::from_str(&text)?;
        metadata.validate()?;
        Ok(metadata)
    }

    fn download_binary_artifact(
        &self,
        metadata: &BinaryArtifactMetadataV1,
        dest: &Path,
    ) -> Result<()> {
        metadata.validate()?;
        let src = self.qualified_blob_file(&metadata.sha256);
        fs::create_dir_all(dest.parent().context("dest has parent")?)?;
        fs::copy(&src, dest).with_context(|| {
            format!(
                "binary artifact {} missing from file registry",
                src.display()
            )
        })?;
        Ok(())
    }

    fn publish_binary_artifact(
        &self,
        meta: &BinaryArtifactPublishMetaV1,
        artifact: &Path,
        _token: Option<&str>,
    ) -> Result<BinaryArtifactMetadataV1> {
        meta.validate().map_err(|error| anyhow!(error))?;
        let package = &meta.manifest.package;
        validate_binary_route_identity(
            &package.org,
            &package.name,
            &package.version,
            &meta.platform.target,
        )?;
        let path = self.binary_artifact_json(
            &package.org,
            &package.name,
            &package.version,
            &meta.platform.target,
            meta.format,
        );
        let blob = self.ensure_qualified_blob(artifact, &meta.sha256, meta.size)?;
        let metadata = BinaryArtifactMetadataV1 {
            schema: BINARY_ARTIFACT_METADATA_SCHEMA_V1.to_owned(),
            org: package.org.clone(),
            name: package.name.clone(),
            version: package.version.clone(),
            platform: meta.platform.clone(),
            format: meta.format,
            sha256: meta.sha256.clone(),
            size: meta.size,
            descriptor_sha256: meta.descriptor_sha256.clone(),
            download_url: format!("/v1/artifacts/{}", meta.sha256),
            published_at: "1970-01-01T00:00:00Z".to_owned(),
            yanked: false,
            source: Some(BinarySourceProvenanceV1 {
                repository: meta.manifest.package.repository.url.clone(),
                vcs_tag: meta.vcs_tag.clone(),
                vcs_commit: meta.vcs_commit.clone(),
            }),
            attachments: meta.attachments.clone(),
        };
        metadata.validate()?;
        let accepted = self.persist_qualified_metadata(&path, &metadata)?;
        ensure!(
            immutable_file_matches(&blob, &accepted.sha256, accepted.size)?,
            "qualified binary metadata exists without its immutable blob"
        );
        Ok(accepted)
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

fn same_url_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn url_is_loopback(url: &reqwest::Url) -> bool {
    matches!(url.host_str(), Some(host) if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost"))
        || url
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_loopback())
}

fn ensure_url_has_no_userinfo(url: &reqwest::Url) -> Result<()> {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("registry download URL must not contain userinfo or a fragment");
    }
    Ok(())
}

fn redacted_url(url: &reqwest::Url) -> String {
    let mut redacted = url.clone();
    redacted.set_query(None);
    redacted.set_fragment(None);
    redacted.to_string()
}

pub struct HttpRegistry {
    base: String,
    client: reqwest::blocking::Client,
    download_client: reqwest::blocking::Client,
}

impl HttpRegistry {
    pub fn new(base: String) -> Result<Self> {
        let parsed =
            reqwest::Url::parse(base.trim_end_matches('/')).context("registry URL is invalid")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("HTTP registry URL must use http or https");
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            bail!("registry URL must not contain credentials, a query, or a fragment");
        }
        let base = parsed.as_str().trim_end_matches('/').to_string();
        let redirect_base = parsed.clone();
        Ok(Self {
            base,
            client: reqwest::blocking::Client::builder()
                .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
                // Authenticated API requests must never replay bearer headers
                // or multipart bodies through a redirect.
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            download_client: reqwest::blocking::Client::builder()
                .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
                .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                    let destination = attempt.url();
                    if attempt.previous().len() >= 10 {
                        return attempt.error("too many artifact download redirects");
                    }
                    let loopback = url_is_loopback(destination);
                    let same_plaintext_registry = redirect_base.scheme() == "http"
                        && same_url_origin(destination, &redirect_base);
                    let clean_authority = destination.username().is_empty()
                        && destination.password().is_none()
                        && destination.fragment().is_none();
                    // A query on an object URL commonly contains an R2/S3
                    // signature. Keep such redirects on that exact origin so
                    // the credential cannot be carried to another host.
                    let signed_redirect_stays_on_origin = attempt
                        .previous()
                        .iter()
                        .filter(|previous| previous.query().is_some())
                        .all(|previous| same_url_origin(destination, previous));
                    if clean_authority
                        && signed_redirect_stays_on_origin
                        && (destination.scheme() == "https"
                            || (destination.scheme() == "http"
                                && (loopback || same_plaintext_registry)))
                    {
                        attempt.follow()
                    } else {
                        attempt.error("refusing unsafe artifact download redirect")
                    }
                }))
                .build()?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn check(response: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        use std::io::Read as _;
        let mut body = Vec::new();
        let mut limited = response.take(MAX_REGISTRY_ERROR_BYTES.saturating_add(1));
        limited.read_to_end(&mut body)?;
        let complete = body.len() as u64 <= MAX_REGISTRY_ERROR_BYTES;
        if complete
            && let Ok(err) = serde_json::from_slice::<zed_interfaces::registry::ApiError>(&body)
        {
            Err(anyhow!(
                "registry error ({status}): {}: {}",
                err.code,
                err.message
            ))
        } else {
            Err(anyhow!("registry error ({status})"))
        }
    }

    /// The registry hands us a `download_url` (possibly a presigned S3/R2
    /// URL on another host). Trusting it verbatim would let a malicious
    /// registry response redirect fetches to plaintext or internal hosts,
    /// so the scheme is checked: https is always fine; http only for loopback
    /// or the exact origin of a registry explicitly configured over http.
    fn allowed_download_url(&self, raw: &str) -> Result<reqwest::Url> {
        let url = reqwest::Url::parse(raw).context("registry returned an invalid download URL")?;
        ensure_url_has_no_userinfo(&url)?;
        let loopback = url_is_loopback(&url);
        let base = reqwest::Url::parse(&self.base).context("registry URL is valid")?;
        match url.scheme() {
            "https" => Ok(url),
            "http" if loopback || (base.scheme() == "http" && same_url_origin(&url, &base)) => {
                Ok(url)
            }
            other => bail!(
                "refusing artifact download over `{other}` from an untrusted origin \
                 (https required outside the configured registry or loopback)"
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

    fn download_to(
        &self,
        raw_url: &str,
        sha256: &str,
        declared_size: u64,
        dest: &Path,
    ) -> Result<()> {
        let url = self.artifact_download_url(raw_url, sha256)?;
        let display_url = redacted_url(&url);
        let response = self.download_client.get(url).send().map_err(|error| {
            anyhow!(
                "artifact download request to {display_url} failed: {}",
                error.without_url()
            )
        })?;
        let status = response.status();
        ensure!(
            status.is_success(),
            "artifact download request to {display_url} failed with HTTP {status}"
        );
        fs::create_dir_all(dest.parent().context("dest has parent")?)?;
        // Bound what we write to disk: the declared size (when sane) plus
        // slack, backed by the global cap. Callers perform exact digest and
        // size verification before promotion.
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
            bail!(
                "artifact exceeded its declared size ({} > {limit} bytes); refusing",
                copied
            );
        }
        Ok(())
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
        self.download_to(&version.download_url, &version.sha256, version.size, dest)
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

    fn get_binary_artifact(
        &self,
        org: &str,
        name: &str,
        version: &str,
        target: &str,
        format: BinaryArchiveFormatV1,
    ) -> Result<BinaryArtifactMetadataV1> {
        validate_binary_route_identity(org, name, version, target)?;
        let response = self
            .client
            .get(self.url(&registry::binary_artifact_path(
                org, name, version, target, format,
            )))
            .send()?;
        let metadata: BinaryArtifactMetadataV1 = Self::check(response)?.json()?;
        metadata.validate()?;
        Ok(metadata)
    }

    fn download_binary_artifact(
        &self,
        metadata: &BinaryArtifactMetadataV1,
        dest: &Path,
    ) -> Result<()> {
        metadata.validate()?;
        self.download_to(
            &metadata.download_url,
            &metadata.sha256,
            metadata.size,
            dest,
        )
    }

    fn publish_binary_artifact(
        &self,
        meta: &BinaryArtifactPublishMetaV1,
        artifact: &Path,
        token: Option<&str>,
    ) -> Result<BinaryArtifactMetadataV1> {
        meta.validate().map_err(|error| anyhow!(error))?;
        let package = &meta.manifest.package;
        validate_binary_route_identity(
            &package.org,
            &package.name,
            &package.version,
            &meta.platform.target,
        )?;
        let form = reqwest::blocking::multipart::Form::new()
            .text(registry::PUBLISH_META_FIELD, serde_json::to_string(meta)?)
            .file(registry::PUBLISH_ARTIFACT_FIELD, artifact)?;
        let mut request = self
            .client
            .put(self.url(&registry::binary_artifact_path(
                &package.org,
                &package.name,
                &package.version,
                &meta.platform.target,
                meta.format,
            )))
            .multipart(form);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let metadata: BinaryArtifactMetadataV1 = Self::check(request.send()?)?.json()?;
        metadata.validate()?;
        Ok(metadata)
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;

    use super::{HttpRegistry, Registry, file_registry_path, file_url_for_path};

    #[test]
    fn remote_file_registry_authority_is_rejected() {
        let error = file_registry_path("file://example.test/registry").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported file registry authority")
        );
    }

    #[test]
    fn registry_base_rejects_embedded_credentials_and_queries() {
        for url in [
            "https://user:secret@registry.example.test",
            "https://registry.example.test?token=secret",
            "https://registry.example.test/#fragment",
        ] {
            assert!(HttpRegistry::new(url.to_owned()).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn qualified_binary_route_keeps_target_out_of_semver() {
        assert_eq!(
            zed_interfaces::registry::binary_artifact_path(
                "acme",
                "tool",
                "1.2.3",
                "aarch64-linux-android",
                zed_interfaces::binary_artifact::BinaryArchiveFormatV1::Zip,
            ),
            "/v1/packages/acme/tool/versions/1.2.3/artifacts/aarch64-linux-android/zip"
        );
        assert!(
            super::validate_binary_route_identity(
                "acme",
                "tool",
                "1.2.3/../../escape",
                "aarch64-linux-android"
            )
            .is_err()
        );
    }

    #[test]
    fn qualified_get_decodes_canonical_binary_metadata() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = server.local_addr().unwrap();
        let response_body = serde_json::json!({
            "schema": "zpkg.binary-artifact-metadata/v1",
            "org": "acme",
            "name": "tool",
            "version": "1.2.3",
            "platform": {
                "target": "aarch64-linux-android",
                "os": "android",
                "arch": "aarch64",
                "abi": "api24"
            },
            "format": "zip",
            "sha256": "a".repeat(64),
            "size": 123,
            "descriptor_sha256": "b".repeat(64),
            "download_url": format!("/v1/artifacts/{}", "a".repeat(64)),
            "published_at": "2026-08-11T16:00:00Z",
            "yanked": false,
            "source": {
                "repository": "https://github.com/acme/tool",
                "vcs_tag": "v1.2.3",
                "vcs_commit": "0123456789abcdef"
            }
        })
        .to_string();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with(
                "GET /v1/packages/acme/tool/versions/1.2.3/artifacts/aarch64-linux-android/zip "
            ));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();
        });

        let registry = HttpRegistry::new(format!("http://{address}")).unwrap();
        let metadata = registry
            .get_binary_artifact(
                "acme",
                "tool",
                "1.2.3",
                "aarch64-linux-android",
                zed_interfaces::binary_artifact::BinaryArchiveFormatV1::Zip,
            )
            .unwrap();
        assert_eq!(metadata.descriptor_sha256, "b".repeat(64));
        assert_eq!(metadata.platform.os, "android");
        thread.join().unwrap();
    }

    #[test]
    fn qualified_publish_json_binds_the_canonical_descriptor() {
        let manifest = zed_interfaces::manifest::Manifest::parse(
            r#"[package]
org = "acme"
name = "tool"
version = "1.2.3"
description = "fixture"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/tool"

[bin]
tool = "bin/tool"
"#,
        )
        .unwrap();
        let platform = zed_interfaces::binary_artifact::BinaryPlatformV1 {
            target: "aarch64-linux-android".to_owned(),
            os: "android".to_owned(),
            arch: "aarch64".to_owned(),
            libc: None,
            abi: Some("api24".to_owned()),
        };
        let archive_sha256 = "a".repeat(64);
        let descriptor_sha256 = "b".repeat(64);
        let publish = zed_interfaces::binary_artifact::BinaryArtifactPublishMetaV1 {
            schema: zed_interfaces::binary_artifact::BINARY_ARTIFACT_PUBLISH_META_SCHEMA_V1
                .to_owned(),
            manifest,
            platform,
            format: zed_interfaces::binary_artifact::BinaryArchiveFormatV1::Zip,
            sha256: archive_sha256,
            size: 123,
            vcs_tag: "v1.2.3".to_owned(),
            vcs_commit: Some("0123456789abcdef".to_owned()),
            descriptor_sha256,
            attachments: Vec::new(),
        };
        publish.validate().unwrap();
        let encoded = serde_json::to_value(publish).unwrap();
        assert_eq!(encoded["schema"], "zpkg.binary-artifact-publish-meta/v1");
        assert_eq!(encoded["descriptor_sha256"], "b".repeat(64));
        assert_eq!(encoded["format"], "zip");
        assert!(encoded.get("attachments").is_none());
    }

    #[test]
    fn authenticated_api_redirects_are_not_followed() {
        let redirect_sink = TcpListener::bind("127.0.0.1:0").unwrap();
        redirect_sink.set_nonblocking(true).unwrap();
        let sink_address = redirect_sink.local_addr().unwrap();
        let api = TcpListener::bind("127.0.0.1:0").unwrap();
        let api_address = api.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = api.accept().unwrap();
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer do-not-forward")
            );
            write!(
                stream,
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{sink_address}/sink\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let registry = HttpRegistry::new(format!("http://{api_address}")).unwrap();
        let error = registry
            .claim_org("acme", Some("do-not-forward"))
            .unwrap_err();
        assert!(format!("{error:#}").contains("307 Temporary Redirect"));
        server.join().unwrap();
        assert!(matches!(
            redirect_sink.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    #[test]
    fn signed_download_redirect_cannot_change_origin_or_leak_its_query() {
        let redirect_sink = TcpListener::bind("127.0.0.1:0").unwrap();
        redirect_sink.set_nonblocking(true).unwrap();
        let sink_address = redirect_sink.local_addr().unwrap();
        let object_server = TcpListener::bind("127.0.0.1:0").unwrap();
        let object_address = object_server.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = object_server.accept().unwrap();
            let mut request = [0_u8; 8192];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.contains("X-Amz-Signature=do-not-log"));
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: http://{sink_address}/stolen?X-Amz-Signature=do-not-log\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });
        let registry = HttpRegistry::new(format!("http://{object_address}")).unwrap();
        let metadata = zed_interfaces::registry::VersionMetadata {
            org: "acme".to_owned(),
            name: "tool".to_owned(),
            version: "1.2.3".to_owned(),
            sha256: "a".repeat(64),
            size: 1,
            format: zed_interfaces::artifact::ArtifactFormat::Zip,
            vcs_tag: "v1.2.3".to_owned(),
            vcs_commit: None,
            download_url: format!("http://{object_address}/object?X-Amz-Signature=do-not-log"),
            published_at: "2026-08-11T16:00:00Z".to_owned(),
            yanked: false,
        };
        let output = tempfile::tempdir().unwrap().path().join("artifact.zip");
        let error = registry.download(&metadata, &output).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("refusing unsafe artifact download redirect"));
        assert!(!message.contains("do-not-log"));
        server.join().unwrap();
        assert!(matches!(
            redirect_sink.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
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
            .artifact_download_url("https://registry.zpkg.net/v1/artifacts/abc123", "abc123")
            .unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:18080/v1/artifacts/abc123");
    }

    #[test]
    fn presigned_external_artifact_url_is_preserved() {
        let registry = HttpRegistry::new("https://registry.zpkg.net".into()).unwrap();
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
