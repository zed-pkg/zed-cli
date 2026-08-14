//! Portable, source-qualified host views over verified package artifacts.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zed_interfaces::binary_artifact::{BINARY_ARCHIVE_ROOT, BINARY_DESCRIPTOR_PATH};
use zed_interfaces::manifest::is_slug;
use zed_lock::{LockClass, LockManager, LockRequest};

use crate::binary_archive::{VerifiedBinaryArtifact, verify_binary_zip};

pub const HOST_LAYOUT_SCHEMA_V1: &str = "zpkg.host-layout/v1";
const HOST_DOWNLOAD_ORIGIN_SCHEMA_V1: &str = "zpkg.host-download-origin/v1";
const DEFAULT_SEGMENT_DELIMITER: &str = "--";
const MAX_LAYOUT_CONFIG_BYTES: u64 = 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const DEFAULT_SOURCES: &[&str] = &["zed", "github", "gitlab", "maven", "npm", "cargo"];

#[derive(Debug, Clone)]
pub struct HostDownloadsLayout {
    root: PathBuf,
    segment_delimiter: String,
    source_precedence: Vec<String>,
    project_index: bool,
    package_index: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPackageCoordinate {
    pub org: String,
    pub project: Option<String>,
    pub package: String,
    pub version: String,
}

#[derive(Debug, Clone)]
pub struct MaterializedHostDownload {
    pub logical_uri: String,
    pub canonical_path: PathBuf,
    pub index_paths: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostLayoutFile {
    schema: String,
    #[serde(default)]
    downloads: DownloadsFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadsFile {
    root: Option<String>,
    segment_delimiter: Option<String>,
    source_precedence: Option<Vec<String>>,
    project_index: Option<bool>,
    package_index: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostDownloadOriginV1 {
    schema: String,
    logical_uri: String,
    org: String,
    project: Option<String>,
    package: String,
    version: String,
    source: String,
    target: String,
    archive_sha256: String,
    archive_size: u64,
    descriptor_sha256: String,
    artifact_path: String,
    package_root: String,
}

impl HostDownloadsLayout {
    /// Load ~/.zpkg/zpkg-config.toml or an explicitly selected equivalent.
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        let user_home = dirs::home_dir()
            .context("could not determine the user home for the Zpkg layout config")?;
        let env_path = std::env::var_os("ZED_PKG_LAYOUT_CONFIG").map(PathBuf::from);
        let explicit = config_path.is_some() || env_path.is_some();
        let path = config_path
            .map(Path::to_path_buf)
            .or(env_path)
            .unwrap_or_else(|| user_home.join(".zpkg").join("zpkg-config.toml"));

        let file = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "host layout config must be a regular, non-symlink file: {}",
                    path.display()
                );
                ensure!(
                    metadata.len() <= MAX_LAYOUT_CONFIG_BYTES,
                    "host layout config exceeds the {MAX_LAYOUT_CONFIG_BYTES}-byte limit"
                );
                let bytes = read_regular_file_bound(&path, metadata.len(), "host layout config")?;
                let text = String::from_utf8(bytes).with_context(|| {
                    format!("host layout config is not UTF-8: {}", path.display())
                })?;
                let parsed: HostLayoutFile = toml::from_str(&text)
                    .with_context(|| format!("parsing host layout config {}", path.display()))?;
                ensure!(
                    parsed.schema == HOST_LAYOUT_SCHEMA_V1,
                    "unsupported host layout schema '{}' (expected '{HOST_LAYOUT_SCHEMA_V1}')",
                    parsed.schema
                );
                parsed.downloads
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    !explicit,
                    "explicit host layout config does not exist: {}",
                    path.display()
                );
                DownloadsFile::default()
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting host layout config {}", path.display()));
            }
        };

        let config_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let root = match file.root.as_deref() {
            Some(raw) => resolve_configured_root(raw, config_dir, &user_home)?,
            None => user_home.join(".zpkg").join("downloads"),
        };
        let segment_delimiter = file
            .segment_delimiter
            .unwrap_or_else(|| DEFAULT_SEGMENT_DELIMITER.to_owned());
        validate_segment_delimiter(&segment_delimiter)?;
        let source_precedence = file.source_precedence.unwrap_or_else(|| {
            DEFAULT_SOURCES
                .iter()
                .map(|source| (*source).to_owned())
                .collect()
        });
        validate_source_precedence(&source_precedence)?;

        Ok(Self {
            root,
            segment_delimiter,
            source_precedence,
            project_index: file.project_index.unwrap_or(true),
            package_index: file.package_index.unwrap_or(true),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn source_precedence(&self) -> &[String] {
        &self.source_precedence
    }

    /// Create immutable org-first, project-first, and package-first views.
    pub fn materialize_binary(
        &self,
        archive: &Path,
        verified: &VerifiedBinaryArtifact,
        project: Option<&str>,
        source: &str,
    ) -> Result<MaterializedHostDownload> {
        ensure!(
            self.source_precedence
                .iter()
                .any(|candidate| candidate == source),
            "download source '{source}' is not listed in source_precedence"
        );
        let coordinate = HostPackageCoordinate::new(
            &verified.manifest.package.org,
            project,
            &verified.manifest.package.name,
            &verified.manifest.package.version,
        )?;
        let target = &verified.descriptor.platform.target;
        validate_safe_segment("binary target", target)?;
        let logical_uri = coordinate.logical_uri(source, Some(target));
        let origin = HostDownloadOriginV1 {
            schema: HOST_DOWNLOAD_ORIGIN_SCHEMA_V1.to_owned(),
            logical_uri: logical_uri.clone(),
            org: coordinate.org.clone(),
            project: coordinate.project.clone(),
            package: coordinate.package.clone(),
            version: coordinate.version.clone(),
            source: source.to_owned(),
            target: target.clone(),
            archive_sha256: verified.sha256.clone(),
            archive_size: verified.size,
            descriptor_sha256: hex::encode(Sha256::digest(
                verified
                    .descriptor
                    .canonical_json_bytes()
                    .map_err(|error| anyhow::anyhow!(error))?,
            )),
            artifact_path: "artifact.zip".to_owned(),
            package_root: BINARY_ARCHIVE_ROOT.to_owned(),
        };

        let root = prepare_root(&self.root)?;
        let relative = self.canonical_relative(&coordinate, source, target);
        let lock_key = hex::encode(Sha256::digest(relative.to_string_lossy().as_bytes()));
        let locks = checked_generated_directory(&root, Path::new(".locks"))?;
        let _guard = LockManager::global().acquire_blocking(
            LockRequest::exclusive(locks.join(format!("{lock_key}.lock")))
                .operation("host download materialization")
                .class(LockClass::Artifact),
        )?;

        let canonical_path =
            materialize_canonical_view(&root, &relative, archive, verified, &origin)?;
        let mut index_paths = Vec::new();
        for alias in self.index_relatives(&coordinate, source, target) {
            if alias != relative {
                index_paths.push(materialize_index_view(
                    &root,
                    &alias,
                    &canonical_path,
                    &origin,
                )?);
            }
        }
        Ok(MaterializedHostDownload {
            logical_uri,
            canonical_path,
            index_paths,
        })
    }

    fn typed(&self, kind: &str, value: &str) -> String {
        format!("zed-{kind}{}{value}", self.segment_delimiter)
    }

    fn canonical_relative(
        &self,
        coordinate: &HostPackageCoordinate,
        source: &str,
        target: &str,
    ) -> PathBuf {
        let mut path = PathBuf::from(self.typed("org", &coordinate.org));
        if let Some(project) = &coordinate.project {
            path.push(self.typed("project", project));
        }
        path.push(self.typed("package", &coordinate.package));
        path.extend(["versions", &coordinate.version, source, "targets", target]);
        path
    }

    fn index_relatives(
        &self,
        coordinate: &HostPackageCoordinate,
        source: &str,
        target: &str,
    ) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if self.project_index
            && let Some(project) = &coordinate.project
        {
            let mut path = PathBuf::from(self.typed("project", project));
            path.push(self.typed("org", &coordinate.org));
            path.push(self.typed("package", &coordinate.package));
            path.extend(["versions", &coordinate.version, source, "targets", target]);
            paths.push(path);
        }
        if self.package_index {
            let mut path = PathBuf::from(self.typed("package", &coordinate.package));
            path.push(self.typed("org", &coordinate.org));
            if let Some(project) = &coordinate.project {
                path.push(self.typed("project", project));
            }
            path.extend(["versions", &coordinate.version, source, "targets", target]);
            paths.push(path);
        }
        paths
    }
}

impl HostPackageCoordinate {
    pub fn new(org: &str, project: Option<&str>, package: &str, version: &str) -> Result<Self> {
        ensure!(is_slug(org), "invalid host package organization '{org}'");
        ensure!(is_slug(package), "invalid host package name '{package}'");
        if let Some(project) = project {
            ensure!(is_slug(project), "invalid host project name '{project}'");
        }
        validate_safe_segment("package version", version)?;
        Ok(Self {
            org: org.to_owned(),
            project: project.map(str::to_owned),
            package: package.to_owned(),
            version: version.to_owned(),
        })
    }

    pub fn logical_uri(&self, source: &str, target: Option<&str>) -> String {
        let mut uri = format!("zed:org:{}/", self.org);
        if let Some(project) = &self.project {
            uri.push_str(&format!("zed:project:{project}/"));
        }
        uri.push_str(&format!(
            "zed:package:{}/versions/{}/{source}",
            self.package, self.version
        ));
        if let Some(target) = target {
            uri.push_str(&format!("/targets/{target}"));
        }
        uri
    }
}

fn resolve_configured_root(raw: &str, config_dir: &Path, user_home: &Path) -> Result<PathBuf> {
    ensure!(!raw.trim().is_empty(), "downloads.root must not be empty");
    let path = if raw == "~" {
        user_home.to_path_buf()
    } else if let Some(relative) = raw.strip_prefix("~/") {
        user_home.join(relative)
    } else {
        let configured = PathBuf::from(raw);
        if configured.is_absolute() {
            configured
        } else {
            config_dir.join(configured)
        }
    };
    ensure_no_parent_components(&path, "downloads.root")?;
    Ok(path)
}

fn ensure_no_parent_components(path: &Path, field: &str) -> Result<()> {
    ensure!(
        !path
            .components()
            .any(|component| component == Component::ParentDir),
        "{field} must not contain parent traversal: {}",
        path.display()
    );
    Ok(())
}

fn validate_segment_delimiter(delimiter: &str) -> Result<()> {
    ensure!(
        !delimiter.is_empty()
            && delimiter.len() <= 8
            && delimiter
                .bytes()
                .all(|byte| matches!(byte, b'-' | b'_' | b'.' | b'+')),
        "segment_delimiter must be 1-8 portable characters from -_.+; colons and path separators are not allowed"
    );
    Ok(())
}

fn validate_source_precedence(sources: &[String]) -> Result<()> {
    ensure!(!sources.is_empty(), "source_precedence must not be empty");
    let mut unique = BTreeSet::new();
    for source in sources {
        ensure!(
            is_slug(source),
            "source_precedence contains invalid source folder '{source}'"
        );
        validate_safe_segment("source folder", source)?;
        ensure!(
            unique.insert(source),
            "source_precedence contains duplicate source '{source}'"
        );
    }
    Ok(())
}

fn validate_safe_segment(field: &str, value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 255
            && !matches!(value, "." | "..")
            && !value.bytes().any(|byte| {
                byte.is_ascii_control()
                    || byte.is_ascii_whitespace()
                    || matches!(
                        byte,
                        b'/' | b'\\' | b':' | b'?' | b'#' | b'%' | b'*' | b'"' | b'<' | b'>' | b'|'
                    )
            })
            && !value.ends_with(['.', ' ']),
        "{field} is not a portable filesystem segment: '{value}'"
    );
    let device_stem = value
        .split_once('.')
        .map_or(value, |(stem, _)| stem)
        .to_ascii_uppercase();
    let numbered_device = device_stem
        .strip_prefix("COM")
        .or_else(|| device_stem.strip_prefix("LPT"))
        .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'));
    ensure!(
        !matches!(
            device_stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$"
        ) && !numbered_device,
        "{field} uses a Windows-reserved device name: '{value}'"
    );
    Ok(())
}

fn prepare_root(root: &Path) -> Result<PathBuf> {
    ensure_no_parent_components(root, "downloads root")?;
    create_private_dir_all(root)?;
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspecting downloads root {}", root.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "downloads root must be a regular, non-symlink directory: {}",
        root.display()
    );
    root.canonicalize()
        .with_context(|| format!("canonicalizing downloads root {}", root.display()))
}

fn checked_generated_directory(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("generated host-download path is not relative and normalized");
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "host-download path contains a non-directory or symlink: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let canonical = current
        .canonicalize()
        .with_context(|| format!("canonicalizing host-download path {}", current.display()))?;
    ensure!(
        canonical.starts_with(root),
        "host-download path escaped configured downloads root"
    );
    Ok(canonical)
}

fn materialize_canonical_view(
    root: &Path,
    relative: &Path,
    archive: &Path,
    verified: &VerifiedBinaryArtifact,
    origin: &HostDownloadOriginV1,
) -> Result<PathBuf> {
    let parent = checked_generated_directory(
        root,
        relative
            .parent()
            .context("host-download destination has a parent")?,
    )?;
    let destination = parent.join(
        relative
            .file_name()
            .context("host-download destination has a final segment")?,
    );
    if existing_view_matches(&destination, origin)? {
        return Ok(destination);
    }

    let staging = tempfile::Builder::new()
        .prefix(".zpkg-stage-")
        .tempdir_in(&parent)
        .context("creating host-download staging directory")?;
    let staged_archive = staging.path().join("artifact.zip");
    copy_archive_bound(
        archive,
        &staged_archive,
        &origin.archive_sha256,
        origin.archive_size,
    )?;
    let staged_verified = verify_binary_zip(&staged_archive, Some(&verified.descriptor.platform))?;
    ensure!(
        staged_verified.sha256 == origin.archive_sha256
            && staged_verified.size == origin.archive_size
            && staged_verified.manifest == verified.manifest
            && staged_verified.descriptor == verified.descriptor,
        "host-download staging verification differs from the previously verified artifact"
    );
    extract_verified_package(&staged_archive, &staged_verified, staging.path())?;
    write_origin(staging.path(), origin)?;
    sync_tree_files(staging.path())?;

    let kept = staging.keep();
    match fs::rename(&kept, &destination) {
        Ok(()) => {
            sync_parent(&destination)?;
            Ok(destination)
        }
        Err(error) if destination.exists() => {
            let _ = fs::remove_dir_all(&kept);
            if existing_view_matches(&destination, origin)? {
                Ok(destination)
            } else {
                Err(error).with_context(|| {
                    format!(
                        "publishing immutable host download {}",
                        destination.display()
                    )
                })
            }
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&kept);
            Err(error).with_context(|| {
                format!(
                    "publishing immutable host download {}",
                    destination.display()
                )
            })
        }
    }
}

fn materialize_index_view(
    root: &Path,
    relative: &Path,
    canonical: &Path,
    origin: &HostDownloadOriginV1,
) -> Result<PathBuf> {
    let parent = checked_generated_directory(
        root,
        relative
            .parent()
            .context("host-download index destination has a parent")?,
    )?;
    let destination = parent.join(
        relative
            .file_name()
            .context("host-download index destination has a final segment")?,
    );
    if existing_view_matches(&destination, origin)? {
        return Ok(destination);
    }
    let staging = tempfile::Builder::new()
        .prefix(".zpkg-index-stage-")
        .tempdir_in(&parent)?;
    replicate_tree(canonical, staging.path())?;
    let kept = staging.keep();
    match fs::rename(&kept, &destination) {
        Ok(()) => {
            sync_parent(&destination)?;
            Ok(destination)
        }
        Err(error) if destination.exists() => {
            let _ = fs::remove_dir_all(&kept);
            if existing_view_matches(&destination, origin)? {
                Ok(destination)
            } else {
                Err(error).with_context(|| {
                    format!(
                        "publishing immutable host-download index {}",
                        destination.display()
                    )
                })
            }
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&kept);
            Err(error).with_context(|| {
                format!(
                    "publishing immutable host-download index {}",
                    destination.display()
                )
            })
        }
    }
}

fn copy_archive_bound(source: &Path, destination: &Path, sha256: &str, size: u64) -> Result<()> {
    let path_metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspecting binary archive {}", source.display()))?;
    ensure!(
        path_metadata.is_file() && !path_metadata.file_type().is_symlink(),
        "binary archive is not a regular, non-symlink file: {}",
        source.display()
    );
    let mut input = fs::File::open(source)
        .with_context(|| format!("opening binary archive {}", source.display()))?;
    let opened = input.metadata()?;
    ensure!(
        opened.is_file(),
        "opened binary archive is not a regular file"
    );
    ensure_same_file(&path_metadata, &opened, source)?;
    ensure!(
        opened.len() == size,
        "binary archive changed size before host materialization"
    );
    let mut output = create_private_file(destination)?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .context("binary archive size overflows while copying")?;
        ensure!(copied <= size, "binary archive grew while being copied");
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
    }
    output.sync_all()?;
    ensure!(
        copied == size && hex::encode(hasher.finalize()) == sha256,
        "binary archive changed while being copied into the host download view"
    );
    Ok(())
}

fn extract_verified_package(
    archive_path: &Path,
    verified: &VerifiedBinaryArtifact,
    destination: &Path,
) -> Result<()> {
    let package_root = destination.join(BINARY_ARCHIVE_ROOT);
    create_private_dir(&package_root)?;
    let file = fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    for expected in &verified.descriptor.files {
        let archive_name = format!("{BINARY_ARCHIVE_ROOT}/{}", expected.path);
        let mut entry = archive
            .by_name(&archive_name)
            .with_context(|| format!("opening verified payload '{archive_name}'"))?;
        let output_path = package_root.join(&expected.path);
        if let Some(parent) = output_path.parent() {
            create_private_dir_all(parent)?;
        }
        let mut output = create_private_file(&output_path)?;
        let mut hasher = Sha256::new();
        let mut copied = 0_u64;
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = entry.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            copied = copied
                .checked_add(read as u64)
                .context("binary payload size overflows while materializing")?;
            ensure!(
                copied <= expected.size,
                "verified binary payload '{}' exceeded its declared size",
                expected.path
            );
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        output.sync_all()?;
        ensure!(
            copied == expected.size && hex::encode(hasher.finalize()) == expected.sha256,
            "verified binary payload '{}' changed during host materialization",
            expected.path
        );
        set_payload_permissions(&output_path, expected.executable)?;
    }
    let descriptor_path = package_root.join(BINARY_DESCRIPTOR_PATH);
    let descriptor_bytes = verified
        .descriptor
        .canonical_json_bytes()
        .map_err(|error| anyhow::anyhow!(error))?;
    let mut descriptor_file = create_private_file(&descriptor_path)?;
    descriptor_file.write_all(&descriptor_bytes)?;
    descriptor_file.sync_all()?;
    Ok(())
}

fn write_origin(destination: &Path, origin: &HostDownloadOriginV1) -> Result<()> {
    let path = destination.join(".zpkg-download.json");
    let mut bytes = serde_json::to_vec_pretty(origin)?;
    bytes.push(b'\n');
    let mut file = create_private_file(&path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn existing_view_matches(path: &Path, expected: &HostDownloadOriginV1) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "refusing conflicting non-directory or symlink host-download path {}",
        path.display()
    );
    let origin_path = path.join(".zpkg-download.json");
    let origin_metadata = fs::symlink_metadata(&origin_path).with_context(|| {
        format!(
            "existing host download lacks binding metadata: {}",
            path.display()
        )
    })?;
    ensure!(
        origin_metadata.is_file()
            && !origin_metadata.file_type().is_symlink()
            && origin_metadata.len() <= MAX_LAYOUT_CONFIG_BYTES,
        "existing host-download binding metadata is unsafe: {}",
        origin_path.display()
    );
    let origin_bytes = read_regular_file_bound(
        &origin_path,
        origin_metadata.len(),
        "host-download binding metadata",
    )?;
    let existing: HostDownloadOriginV1 = serde_json::from_slice(&origin_bytes)?;
    ensure!(
        existing == *expected,
        "refusing to overwrite immutable host download {} with different source, target, or bytes",
        path.display()
    );
    let verified = verify_binary_zip(&path.join(&existing.artifact_path), None)
        .with_context(|| format!("re-verifying existing host download {}", path.display()))?;
    ensure!(
        verified.sha256 == existing.archive_sha256
            && verified.size == existing.archive_size
            && verified.descriptor.platform.target == existing.target,
        "existing host download no longer matches its immutable binding metadata"
    );
    verify_materialized_package(path, &verified)?;
    Ok(true)
}

fn verify_materialized_package(view: &Path, verified: &VerifiedBinaryArtifact) -> Result<()> {
    let package_root = view.join(BINARY_ARCHIVE_ROOT);
    let root_metadata = fs::symlink_metadata(&package_root)
        .with_context(|| format!("inspecting materialized package {}", package_root.display()))?;
    ensure!(
        root_metadata.is_dir() && !root_metadata.file_type().is_symlink(),
        "materialized package root is not a regular directory"
    );

    let descriptor_path = package_root.join(BINARY_DESCRIPTOR_PATH);
    let descriptor_bytes = verified
        .descriptor
        .canonical_json_bytes()
        .map_err(|error| anyhow::anyhow!(error))?;
    let actual_descriptor = read_regular_file_bound(
        &descriptor_path,
        descriptor_bytes.len() as u64,
        "materialized binary descriptor",
    )?;
    ensure!(
        actual_descriptor == descriptor_bytes,
        "materialized binary descriptor differs from the verified archive"
    );

    let mut expected_paths = BTreeSet::from([BINARY_DESCRIPTOR_PATH.to_owned()]);
    for expected in &verified.descriptor.files {
        let path = package_root.join(&expected.path);
        verify_materialized_payload(&path, &expected.sha256, expected.size, expected.executable)?;
        expected_paths.insert(expected.path.clone());
    }

    let mut actual_paths = BTreeSet::new();
    for entry in WalkDir::new(&package_root).follow_links(false) {
        let entry = entry?;
        let relative = entry.path().strip_prefix(&package_root)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "materialized package contains a symlink: {}",
            entry.path().display()
        );
        if metadata.is_dir() {
            continue;
        }
        ensure!(
            metadata.is_file(),
            "materialized package contains a special file: {}",
            entry.path().display()
        );
        let relative = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        actual_paths.insert(relative);
    }
    ensure!(
        actual_paths == expected_paths,
        "materialized package file inventory differs from the verified archive"
    );
    Ok(())
}

fn read_regular_file_bound(path: &Path, expected_size: u64, kind: &str) -> Result<Vec<u8>> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {kind} {}", path.display()))?;
    ensure!(
        path_metadata.is_file() && !path_metadata.file_type().is_symlink(),
        "{kind} is not a regular, non-symlink file: {}",
        path.display()
    );
    let file = fs::File::open(path)?;
    let opened = file.metadata()?;
    ensure_same_file(&path_metadata, &opened, path)?;
    ensure!(
        opened.len() == expected_size,
        "{kind} size differs from the verified archive"
    );
    let capacity = usize::try_from(expected_size).context("verified file exceeds this platform")?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(expected_size.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == expected_size,
        "{kind} changed while it was being read"
    );
    Ok(bytes)
}

fn verify_materialized_payload(
    path: &Path,
    expected_sha256: &str,
    expected_size: u64,
    expected_executable: bool,
) -> Result<()> {
    let bytes = read_regular_file_bound(path, expected_size, "materialized binary payload")?;
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == expected_sha256,
        "materialized binary payload digest differs from the verified archive: {}",
        path.display()
    );
    ensure_payload_permissions(path, expected_executable)
}

#[cfg(unix)]
fn ensure_payload_permissions(path: &Path, expected_executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let executable = fs::metadata(path)?.permissions().mode() & 0o111 != 0;
    ensure!(
        executable == expected_executable,
        "materialized payload executable intent differs from the verified archive: {}",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn ensure_payload_permissions(_path: &Path, _expected_executable: bool) -> Result<()> {
    Ok(())
}

fn replicate_tree(source: &Path, destination: &Path) -> Result<()> {
    for entry in WalkDir::new(source).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        let relative = entry.path().strip_prefix(source)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "canonical host download unexpectedly contains a symlink"
        );
        if metadata.is_dir() {
            create_private_dir(&target)?;
        } else {
            ensure!(
                metadata.is_file(),
                "canonical host download contains a special file"
            );
            if fs::hard_link(entry.path(), &target).is_err() {
                fs::copy(entry.path(), &target)?;
            }
        }
    }
    sync_tree_files(destination)
}

fn sync_tree_files(root: &Path) -> Result<()> {
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "host download contains a symlink"
        );
        if metadata.is_file() {
            sync_regular_file(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn sync_regular_file(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
fn sync_regular_file(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> Result<fs::File> {
    Ok(fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?)
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    Ok(())
}

fn create_private_dir_all(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "directory path is not a regular, non-symlink directory: {}",
                path.display()
            );
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut missing = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        missing.push(cursor.to_path_buf());
        cursor = cursor
            .parent()
            .context("directory path has no existing ancestor")?;
    }
    let ancestor = fs::symlink_metadata(cursor)?;
    ensure!(
        ancestor.is_dir() && !ancestor.file_type().is_symlink(),
        "directory ancestor is not a regular directory: {}",
        cursor.display()
    );
    for directory in missing.into_iter().rev() {
        create_private_dir(&directory)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_payload_permissions(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o700 } else { 0o600 }),
    )?;
    Ok(())
}

#[cfg(not(unix))]
fn set_payload_permissions(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ensure_same_file(before: &fs::Metadata, opened: &fs::Metadata, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        before.dev() == opened.dev() && before.ino() == opened.ino(),
        "binary archive changed while being opened: {}",
        path.display()
    );
    Ok(())
}

#[cfg(not(unix))]
fn ensure_same_file(_before: &fs::Metadata, _opened: &fs::Metadata, _path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("host-download path has a parent")?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary_archive::{BinaryPackOptions, pack_binary_zip};
    use zed_interfaces::binary_artifact::BinaryPlatformV1;
    use zed_interfaces::paths::MANIFEST_FILE;

    fn fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        VerifiedBinaryArtifact,
        PathBuf,
    ) {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("bin")).unwrap();
        fs::write(
            project.path().join(MANIFEST_FILE),
            r#"[package]
org = "acme"
name = "hello"
version = "1.2.3"
description = "fixture"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/hello"

[bin]
hello = "bin/hello"
"#,
        )
        .unwrap();
        fs::write(project.path().join("bin/hello"), b"hello\n").unwrap();
        let output = tempfile::tempdir().unwrap();
        let packed = pack_binary_zip(
            project.path(),
            &BinaryPackOptions {
                platform: BinaryPlatformV1 {
                    target: "aarch64-linux-android".to_owned(),
                    os: "android".to_owned(),
                    arch: "aarch64".to_owned(),
                    libc: None,
                    abi: Some("api24".to_owned()),
                },
                includes: Vec::new(),
                out_dir: Some(output.path().to_path_buf()),
                vcs_commit: Some("0123456789abcdef".to_owned()),
            },
        )
        .unwrap();
        let verified = verify_binary_zip(&packed.packed.path, None).unwrap();
        (project, output, verified, packed.packed.path)
    }

    fn layout(root: &Path) -> HostDownloadsLayout {
        HostDownloadsLayout {
            root: root.to_path_buf(),
            segment_delimiter: "--".to_owned(),
            source_precedence: DEFAULT_SOURCES
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            project_index: true,
            package_index: true,
        }
    }

    #[test]
    fn project_and_projectless_paths_are_unambiguous_and_portable() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let project =
            HostPackageCoordinate::new("acme", Some("payments"), "hello", "1.2.3").unwrap();
        assert_eq!(
            layout.canonical_relative(&project, "zed", "aarch64-linux-android"),
            PathBuf::from(
                "zed-org--acme/zed-project--payments/zed-package--hello/versions/1.2.3/zed/targets/aarch64-linux-android"
            )
        );
        assert_eq!(
            project.logical_uri("zed", Some("aarch64-linux-android")),
            "zed:org:acme/zed:project:payments/zed:package:hello/versions/1.2.3/zed/targets/aarch64-linux-android"
        );
        let projectless = HostPackageCoordinate::new("acme", None, "hello", "1.2.3").unwrap();
        assert_eq!(
            layout.canonical_relative(&projectless, "zed", "aarch64-linux-android"),
            PathBuf::from(
                "zed-org--acme/zed-package--hello/versions/1.2.3/zed/targets/aarch64-linux-android"
            )
        );
    }

    #[test]
    fn materialization_creates_real_source_and_index_directories_idempotently() {
        let (_project, _output, verified, archive) = fixture();
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let first = layout
            .materialize_binary(&archive, &verified, Some("payments"), "zed")
            .unwrap();
        let second = layout
            .materialize_binary(&archive, &verified, Some("payments"), "zed")
            .unwrap();
        assert_eq!(first.canonical_path, second.canonical_path);
        assert!(first.canonical_path.is_dir());
        assert!(first.canonical_path.join("artifact.zip").is_file());
        assert!(first.canonical_path.join("pkg/.zpkg.toml").is_file());
        assert!(first.canonical_path.join("pkg/bin/hello").is_file());
        assert_eq!(first.index_paths.len(), 2);
        assert!(first.index_paths.iter().all(|path| path.is_dir()));
    }

    #[test]
    fn same_source_target_version_is_immutable_but_other_sources_are_separate() {
        let (project, _output, verified, archive) = fixture();
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        layout
            .materialize_binary(&archive, &verified, None, "zed")
            .unwrap();

        fs::write(project.path().join("bin/hello"), b"different\n").unwrap();
        let output = tempfile::tempdir().unwrap();
        let changed = pack_binary_zip(
            project.path(),
            &BinaryPackOptions {
                platform: verified.descriptor.platform.clone(),
                includes: Vec::new(),
                out_dir: Some(output.path().to_path_buf()),
                vcs_commit: Some("fedcba9876543210".to_owned()),
            },
        )
        .unwrap();
        let changed_verified = verify_binary_zip(&changed.packed.path, None).unwrap();
        let conflict = layout
            .materialize_binary(&changed.packed.path, &changed_verified, None, "zed")
            .unwrap_err();
        assert!(format!("{conflict:#}").contains("immutable host download"));
        layout
            .materialize_binary(&changed.packed.path, &changed_verified, None, "github")
            .unwrap();
    }

    #[test]
    fn config_rejects_unsafe_layout_controls() {
        assert!(validate_segment_delimiter(":").is_err());
        assert!(
            resolve_configured_root(
                "../escape",
                Path::new("/tmp/.zpkg"),
                Path::new("/home/user")
            )
            .is_err()
        );
        assert!(validate_source_precedence(&["zed".into(), "zed".into()]).is_err());
        assert!(validate_source_precedence(&["con".into()]).is_err());
        assert!(validate_source_precedence(&["NUL.txt".into()]).is_err());
        assert!(
            toml::from_str::<HostLayoutFile>(
                "schema = 'zpkg.host-layout/v1'\n[downloads]\nunknown = true\n"
            )
            .is_err()
        );
    }

    #[test]
    fn explicit_config_resolves_relative_roots_and_applies_indexes() {
        let config_dir = tempfile::tempdir().unwrap();
        let config = config_dir.path().join("zpkg-config.toml");
        fs::write(
            &config,
            r#"schema = "zpkg.host-layout/v1"

[downloads]
root = "host-cache"
segment_delimiter = "__"
source_precedence = ["github", "zed"]
project_index = false
package_index = true
"#,
        )
        .unwrap();

        let layout = HostDownloadsLayout::load(Some(&config)).unwrap();
        assert_eq!(layout.root, config_dir.path().join("host-cache"));
        assert_eq!(layout.segment_delimiter, "__");
        assert_eq!(layout.source_precedence, ["github", "zed"]);
        assert!(!layout.project_index);
        assert!(layout.package_index);
    }

    #[cfg(unix)]
    #[test]
    fn explicit_config_rejects_even_a_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let config_dir = tempfile::tempdir().unwrap();
        let config = config_dir.path().join("zpkg-config.toml");
        symlink(config_dir.path().join("missing"), &config).unwrap();
        let error = HostDownloadsLayout::load(Some(&config)).unwrap_err();
        assert!(format!("{error:#}").contains("regular, non-symlink"));
    }

    #[test]
    fn idempotent_materialization_rejects_tampered_payload_and_binding_metadata() {
        let (_project, _output, verified, archive) = fixture();
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let materialized = layout
            .materialize_binary(&archive, &verified, Some("payments"), "zed")
            .unwrap();

        let payload = materialized.canonical_path.join("pkg/bin/hello");
        fs::write(&payload, b"evil!\n").unwrap();
        let error = layout
            .materialize_binary(&archive, &verified, Some("payments"), "zed")
            .unwrap_err();
        assert!(format!("{error:#}").contains("payload digest differs"));

        fs::write(&payload, b"hello\n").unwrap();
        let origin = materialized.canonical_path.join(".zpkg-download.json");
        let mut bytes = fs::read(&origin).unwrap();
        bytes.extend_from_slice(b"{}");
        fs::write(&origin, bytes).unwrap();
        let error = layout
            .materialize_binary(&archive, &verified, Some("payments"), "zed")
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("trailing"), "{message}");
    }
}
