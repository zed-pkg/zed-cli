use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Read};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use zed_interfaces::{
    OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor, OciDigest, OciReference,
    ZED_OCI_BINARY_MEDIA_TYPE_V1, ZED_OCI_CONFIG_MEDIA_TYPE_V1, ZED_OCI_LOCK_MEDIA_TYPE_V1,
    ZED_OCI_MANIFEST_MEDIA_TYPE_V1, ZED_OCI_PACKAGE_TAR_GZ_MEDIA_TYPE_V1,
    ZED_OCI_PACKAGE_ZIP_MEDIA_TYPE_V1,
};

use crate::interactive;

pub const OCI_PUSH_RESULT_SCHEMA_V1: &str = "zed.oci-push-result/v1";
const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const OCI_LAYOUT_VERSION: &str = "1.0.0";
const OCI_LAYOUT_FILE: &str = "oci-layout";
const OCI_INDEX_FILE: &str = "index.json";
const OCI_REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;

#[derive(Debug)]
pub struct OciPushOptions<'a> {
    pub layout: &'a Path,
    pub destination: &'a str,
    pub oras: &'a Path,
    pub username: Option<&'a str>,
    pub password_stdin: bool,
    pub registry_config: Option<&'a Path>,
    pub anonymous: bool,
    pub plain_http: bool,
    pub insecure_tls: bool,
    pub ca_file: Option<&'a Path>,
    pub allow_tag_replacement: bool,
    pub interactive: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OciPushStatus {
    Pushed,
    Replaced,
    AlreadyPresent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OciAuthentication {
    PasswordStdin,
    RegistryConfig,
    Anonymous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OciPushResult {
    pub schema: String,
    pub layout: String,
    pub destination: OciReference,
    pub manifest: OciDescriptor,
    pub status: OciPushStatus,
    pub authentication: OciAuthentication,
    pub transport: String,
    pub oras_version: String,
    pub blob_count: usize,
    pub total_blob_bytes: u64,
    pub plain_http: bool,
    pub insecure_tls: bool,
}

#[derive(Debug)]
struct VerifiedLayout {
    path: PathBuf,
    tag: String,
    manifest: OciDescriptor,
    blob_count: usize,
    total_blob_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciLayoutVersion {
    image_layout_version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciImageIndex {
    schema_version: u32,
    media_type: String,
    manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciImageManifest {
    schema_version: u32,
    media_type: String,
    artifact_type: String,
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
}

#[derive(Debug, Serialize)]
struct DockerRegistryConfig {
    auths: BTreeMap<String, DockerRegistryAuth>,
}

#[derive(Debug, Serialize)]
struct DockerRegistryAuth {
    auth: String,
}

struct PreparedRegistryConfig {
    path: PathBuf,
    authentication: OciAuthentication,
    _temporary: Option<TempDir>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemoteTag {
    Missing,
    Digest(OciDigest),
}

pub fn push(options: OciPushOptions<'_>) -> Result<()> {
    let password = if options.password_stdin {
        Some(read_password()?)
    } else {
        None
    };
    let result = execute(&options, password.as_deref())?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("OCI registry push");
        println!("layout: {}", result.layout);
        println!("destination: {}", result.destination);
        println!("manifest: {}", result.manifest.digest);
        println!("status: {}", push_status_name(result.status));
        println!("transport: {} ({})", result.transport, result.oras_version);
        println!(
            "authentication: {}",
            authentication_name(result.authentication)
        );
        println!("blobs: {}", result.blob_count);
        println!("blob bytes: {}", result.total_blob_bytes);
        if result.plain_http {
            println!("transport security: loopback plain HTTP explicitly enabled");
        } else if result.insecure_tls {
            println!("transport security: TLS certificate verification explicitly disabled");
        }
    }
    Ok(())
}

fn execute(options: &OciPushOptions<'_>, password: Option<&str>) -> Result<OciPushResult> {
    let mut destination = parse_destination(options.destination)?;
    validate_transport_options(options, &destination)?;
    let layout = verify_layout(options.layout, &destination)?;
    let oras_version = validate_oras(options.oras, options.ca_file.is_some())?;
    let registry_config = prepare_registry_config(options, &destination, password)?;
    let target = registry_target(&destination)?;

    let remote = resolve_remote(options.oras, &registry_config.path, &target, options)?;
    let status = match remote {
        RemoteTag::Digest(ref digest) if digest == &layout.manifest.digest => {
            OciPushStatus::AlreadyPresent
        }
        RemoteTag::Digest(ref digest) if !options.allow_tag_replacement => {
            bail!(
                "refusing to replace OCI tag `{}`: remote digest is {}, verified layout digest is {}; pass --allow-tag-replacement to replace it explicitly",
                destination.tag.as_deref().unwrap_or_default(),
                digest,
                layout.manifest.digest
            );
        }
        RemoteTag::Digest(_) => OciPushStatus::Replaced,
        RemoteTag::Missing => OciPushStatus::Pushed,
    };

    if status != OciPushStatus::AlreadyPresent {
        interactive::confirm(
            options.interactive,
            &format!(
                "{} {} with verified manifest {} through ORAS",
                match status {
                    OciPushStatus::Pushed => "push",
                    OciPushStatus::Replaced => "replace the existing tag at",
                    OciPushStatus::AlreadyPresent => unreachable!(),
                },
                target,
                layout.manifest.digest
            ),
        )?;
        copy_layout(
            options.oras,
            &registry_config.path,
            &layout,
            &target,
            options,
        )?;
        let verified_remote =
            resolve_remote(options.oras, &registry_config.path, &target, options)?;
        match verified_remote {
            RemoteTag::Digest(ref digest) if digest == &layout.manifest.digest => {}
            RemoteTag::Digest(digest) => bail!(
                "OCI registry verification failed after ORAS copy: expected {}, resolved {}",
                layout.manifest.digest,
                digest
            ),
            RemoteTag::Missing => bail!(
                "OCI registry verification failed after ORAS copy: destination tag is still missing"
            ),
        }
    }

    destination.digest = Some(layout.manifest.digest.clone());
    Ok(OciPushResult {
        schema: OCI_PUSH_RESULT_SCHEMA_V1.to_string(),
        layout: layout.path.display().to_string(),
        destination,
        manifest: layout.manifest,
        status,
        authentication: registry_config.authentication,
        transport: "oras-cp".to_string(),
        oras_version,
        blob_count: layout.blob_count,
        total_blob_bytes: layout.total_blob_bytes,
        plain_http: options.plain_http,
        insecure_tls: options.insecure_tls,
    })
}

fn parse_destination(value: &str) -> Result<OciReference> {
    let reference = OciReference::parse(value).map_err(|error| anyhow::anyhow!(error))?;
    if reference.tag.is_none() {
        bail!("OCI push destination requires an explicit tag");
    }
    if reference.digest.is_some() {
        bail!(
            "OCI push destination must not preselect a digest; the verified layout determines the immutable digest"
        );
    }
    Ok(reference)
}

fn validate_transport_options(
    options: &OciPushOptions<'_>,
    destination: &OciReference,
) -> Result<()> {
    if options.plain_http && (options.insecure_tls || options.ca_file.is_some()) {
        bail!("--plain-http cannot be combined with --insecure-tls or --ca-file");
    }
    if options.insecure_tls && options.ca_file.is_some() {
        bail!("--insecure-tls cannot be combined with --ca-file");
    }
    if options.plain_http && !is_loopback_registry(&destination.registry) {
        bail!(
            "--plain-http is accepted only for loopback registries; `{}` is not loopback",
            destination.registry
        );
    }
    if let Some(ca_file) = options.ca_file {
        let metadata = fs::symlink_metadata(ca_file)
            .with_context(|| format!("read OCI registry CA file {}", ca_file.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "OCI registry CA path {} must be a regular non-symlink file",
                ca_file.display()
            );
        }
    }
    Ok(())
}

fn verify_layout(layout: &Path, destination: &OciReference) -> Result<VerifiedLayout> {
    let path = fs::canonicalize(layout)
        .with_context(|| format!("resolve OCI image layout {}", layout.display()))?;
    require_real_directory(&path, "OCI image layout")?;
    require_real_directory(&path.join("blobs"), "OCI blobs directory")?;
    require_real_directory(&path.join("blobs/sha256"), "OCI SHA-256 blobs directory")?;

    let layout_version: OciLayoutVersion = read_json_file(&path.join(OCI_LAYOUT_FILE))?;
    if layout_version.image_layout_version != OCI_LAYOUT_VERSION {
        bail!(
            "unsupported OCI image-layout version `{}`; expected `{}`",
            layout_version.image_layout_version,
            OCI_LAYOUT_VERSION
        );
    }

    let index: OciImageIndex = read_json_file(&path.join(OCI_INDEX_FILE))?;
    if index.schema_version != 2 || index.media_type != OCI_IMAGE_INDEX_MEDIA_TYPE {
        bail!(
            "invalid OCI index: expected schemaVersion 2 and mediaType `{}`",
            OCI_IMAGE_INDEX_MEDIA_TYPE
        );
    }
    if index.manifests.len() != 1 {
        bail!(
            "Zed OCI push requires an image layout with exactly one manifest, found {}",
            index.manifests.len()
        );
    }
    let manifest_descriptor = index.manifests.into_iter().next().unwrap();
    manifest_descriptor
        .validate("OCI image index manifest")
        .map_err(|error| anyhow::anyhow!(error))?;
    if manifest_descriptor.media_type != OCI_IMAGE_MANIFEST_MEDIA_TYPE {
        bail!(
            "OCI image index manifest requires media type `{}`",
            OCI_IMAGE_MANIFEST_MEDIA_TYPE
        );
    }
    let tag = manifest_descriptor
        .annotations
        .get(OCI_REF_NAME_ANNOTATION)
        .cloned()
        .context("OCI image index manifest is missing org.opencontainers.image.ref.name")?;
    if destination.tag.as_deref() != Some(tag.as_str()) {
        bail!(
            "OCI layout tag `{}` does not match destination tag `{}`",
            tag,
            destination.tag.as_deref().unwrap_or_default()
        );
    }

    let manifest_bytes = verify_descriptor_blob(&path, &manifest_descriptor, true)?
        .context("OCI manifest bytes were not captured")?;
    let manifest: OciImageManifest =
        serde_json::from_slice(&manifest_bytes).context("parse verified OCI image manifest")?;
    if manifest.schema_version != 2
        || manifest.media_type != OCI_IMAGE_MANIFEST_MEDIA_TYPE
        || manifest.artifact_type != ZED_OCI_CONFIG_MEDIA_TYPE_V1
    {
        bail!(
            "invalid Zed OCI manifest: expected schemaVersion 2, mediaType `{}`, and artifactType `{}`",
            OCI_IMAGE_MANIFEST_MEDIA_TYPE,
            ZED_OCI_CONFIG_MEDIA_TYPE_V1
        );
    }
    if manifest.config.media_type != ZED_OCI_CONFIG_MEDIA_TYPE_V1 {
        bail!(
            "Zed OCI config requires media type `{}`",
            ZED_OCI_CONFIG_MEDIA_TYPE_V1
        );
    }
    if manifest.layers.is_empty() {
        bail!("Zed OCI manifest must contain package and metadata layers");
    }

    let mut primary_packages = 0usize;
    let mut source_manifests = 0usize;
    let mut lockfiles = 0usize;
    for layer in &manifest.layers {
        match layer.media_type.as_str() {
            ZED_OCI_PACKAGE_TAR_GZ_MEDIA_TYPE_V1
            | ZED_OCI_PACKAGE_ZIP_MEDIA_TYPE_V1
            | ZED_OCI_BINARY_MEDIA_TYPE_V1 => primary_packages += 1,
            ZED_OCI_MANIFEST_MEDIA_TYPE_V1 => source_manifests += 1,
            ZED_OCI_LOCK_MEDIA_TYPE_V1 => lockfiles += 1,
            other => bail!("unsupported Zed OCI layer media type `{other}`"),
        }
    }
    if primary_packages != 1 || source_manifests != 1 || lockfiles > 1 {
        bail!(
            "Zed OCI manifest requires exactly one package layer, exactly one source-manifest layer, and at most one lockfile layer; found package={}, manifest={}, lockfile={}",
            primary_packages,
            source_manifests,
            lockfiles
        );
    }

    let mut expected_blobs = BTreeMap::new();
    record_expected_blob(&mut expected_blobs, &manifest_descriptor, "OCI manifest")?;
    record_expected_blob(&mut expected_blobs, &manifest.config, "OCI config")?;
    verify_descriptor_blob(&path, &manifest.config, false)?;
    for (index, layer) in manifest.layers.iter().enumerate() {
        record_expected_blob(&mut expected_blobs, layer, &format!("OCI layer {index}"))?;
        verify_descriptor_blob(&path, layer, false)?;
    }
    verify_exact_blob_set(&path, &expected_blobs)?;

    let total_blob_bytes = expected_blobs.values().try_fold(0u64, |total, size| {
        total
            .checked_add(*size)
            .context("OCI layout total blob byte count overflow")
    })?;
    Ok(VerifiedLayout {
        path,
        tag,
        manifest: manifest_descriptor,
        blob_count: expected_blobs.len(),
        total_blob_bytes,
    })
}

fn require_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("read {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} {} must be a real directory", path.display());
    }
    Ok(())
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read OCI metadata file {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "OCI metadata path {} must be a regular non-symlink file",
            path.display()
        );
    }
    let bytes = fs::read(path).with_context(|| format!("read OCI metadata {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse OCI metadata {}", path.display()))
}

fn record_expected_blob(
    expected: &mut BTreeMap<String, u64>,
    descriptor: &OciDescriptor,
    label: &str,
) -> Result<()> {
    descriptor
        .validate(label)
        .map_err(|error| anyhow::anyhow!(error))?;
    let digest = descriptor.digest.to_string();
    if let Some(previous_size) = expected.insert(digest.clone(), descriptor.size) {
        bail!(
            "OCI digest `{digest}` is declared more than once (sizes {previous_size} and {})",
            descriptor.size
        );
    }
    Ok(())
}

fn verify_descriptor_blob(
    layout: &Path,
    descriptor: &OciDescriptor,
    capture: bool,
) -> Result<Option<Vec<u8>>> {
    descriptor
        .validate("OCI blob descriptor")
        .map_err(|error| anyhow::anyhow!(error))?;
    let encoded = descriptor
        .digest
        .encoded()
        .context("OCI descriptor is not a canonical SHA-256 digest")?;
    let path = layout.join("blobs/sha256").join(encoded);
    let metadata =
        fs::symlink_metadata(&path).with_context(|| format!("read OCI blob {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "OCI blob {} must be a regular non-symlink file",
            path.display()
        );
    }
    if metadata.len() != descriptor.size {
        bail!(
            "OCI blob {} size drift: descriptor={}, actual={}",
            descriptor.digest,
            descriptor.size,
            metadata.len()
        );
    }
    let canonical =
        fs::canonicalize(&path).with_context(|| format!("resolve OCI blob {}", path.display()))?;
    if !canonical.starts_with(layout) {
        bail!("OCI blob {} escapes the image layout", path.display());
    }

    let mut file =
        fs::File::open(&path).with_context(|| format!("open OCI blob {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut captured = capture.then(Vec::new);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("hash OCI blob {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if let Some(bytes) = &mut captured {
            bytes.extend_from_slice(&buffer[..read]);
        }
    }
    let actual = OciDigest::parse(format!("sha256:{}", hex::encode(hasher.finalize())))
        .map_err(|error| anyhow::anyhow!(error))?;
    if actual != descriptor.digest {
        bail!(
            "OCI blob digest drift: descriptor={}, actual={}",
            descriptor.digest,
            actual
        );
    }
    Ok(captured)
}

fn verify_exact_blob_set(layout: &Path, expected: &BTreeMap<String, u64>) -> Result<()> {
    let directory = layout.join("blobs/sha256");
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(&directory)
        .with_context(|| format!("read OCI blob directory {}", directory.display()))?
    {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_symlink()
            || !metadata.is_file()
            || name.len() != 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            bail!(
                "unexpected OCI blob entry {}; expected regular lowercase SHA-256 files only",
                entry.path().display()
            );
        }
        actual.insert(format!("sha256:{name}"));
    }
    let expected = expected.keys().cloned().collect::<BTreeSet<_>>();
    if actual != expected {
        let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
        let unexpected = actual.difference(&expected).cloned().collect::<Vec<_>>();
        bail!(
            "OCI image layout blob set does not match the verified manifest; missing={missing:?}, unexpected={unexpected:?}"
        );
    }
    Ok(())
}

fn prepare_registry_config(
    options: &OciPushOptions<'_>,
    destination: &OciReference,
    password: Option<&str>,
) -> Result<PreparedRegistryConfig> {
    let selected = usize::from(options.registry_config.is_some())
        + usize::from(options.anonymous)
        + usize::from(options.username.is_some() || options.password_stdin);
    if selected != 1 {
        bail!(
            "choose exactly one OCI authentication mode: --username with --password-stdin, --registry-config, or --anonymous"
        );
    }

    if let Some(path) = options.registry_config {
        if password.is_some() {
            bail!("an explicit registry config cannot be combined with a password");
        }
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("read OCI registry config {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "OCI registry config {} must be a regular non-symlink file",
                path.display()
            );
        }
        return Ok(PreparedRegistryConfig {
            path: fs::canonicalize(path)
                .with_context(|| format!("resolve OCI registry config {}", path.display()))?,
            authentication: OciAuthentication::RegistryConfig,
            _temporary: None,
        });
    }

    let temporary = tempfile::Builder::new()
        .prefix("zed-oci-auth-")
        .tempdir()
        .context("create temporary ORAS registry config directory")?;
    let path = temporary.path().join("config.json");
    let (config, authentication) = if options.anonymous {
        if password.is_some() {
            bail!("anonymous OCI push cannot receive a password");
        }
        (
            DockerRegistryConfig {
                auths: BTreeMap::new(),
            },
            OciAuthentication::Anonymous,
        )
    } else {
        let username = options
            .username
            .context("--username is required with --password-stdin")?;
        validate_username(username)?;
        let password = password.context("--password-stdin did not yield a password")?;
        if password.is_empty() || password.contains('\0') {
            bail!("OCI registry password must be non-empty and contain no NUL byte");
        }
        let mut auths = BTreeMap::new();
        auths.insert(
            destination.registry.clone(),
            DockerRegistryAuth {
                auth: encode_base64(format!("{username}:{password}").as_bytes()),
            },
        );
        (
            DockerRegistryConfig { auths },
            OciAuthentication::PasswordStdin,
        )
    };
    write_secure_file(&path, &serde_json::to_vec(&config)?)?;
    Ok(PreparedRegistryConfig {
        path,
        authentication,
        _temporary: Some(temporary),
    })
}

fn validate_username(username: &str) -> Result<()> {
    if username.is_empty()
        || username.trim() != username
        || username.contains(':')
        || username.chars().any(char::is_control)
    {
        bail!(
            "OCI registry username must be non-empty, trimmed, contain no colon, and contain no control character"
        );
    }
    Ok(())
}

fn write_secure_file(path: &Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("create temporary ORAS registry config {}", path.display()))?;
        file.write_all(bytes)?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes)
            .with_context(|| format!("create temporary ORAS registry config {}", path.display()))?;
    }
    Ok(())
}

fn read_password() -> Result<String> {
    let mut password = if io::stdin().is_terminal() {
        rpassword::prompt_password("OCI registry password or token: ")?
    } else {
        let mut password = String::new();
        io::stdin()
            .lock()
            .read_line(&mut password)
            .context("read OCI registry password from stdin")?;
        while password.ends_with('\n') || password.ends_with('\r') {
            password.pop();
        }
        password
    };
    while password.ends_with('\n') || password.ends_with('\r') {
        password.pop();
    }
    if password.is_empty() || password.contains('\0') {
        bail!("OCI registry password must be non-empty and contain no NUL byte");
    }
    Ok(password)
}

fn validate_oras(oras: &Path, require_ca_file: bool) -> Result<String> {
    let version_output = Command::new(oras)
        .arg("version")
        .stdin(Stdio::null())
        .output()
        .with_context(|| {
            format!(
                "run ORAS executable `{}`; install ORAS 1.2 or newer or pass --oras",
                oras.display()
            )
        })?;
    require_success("oras version", &version_output)?;
    let version = combined_output(&version_output);
    if version.trim().is_empty() {
        bail!("ORAS version command returned no version information");
    }

    let cp_help = Command::new(oras)
        .args(["cp", "--help"])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("inspect ORAS cp support through `{}`", oras.display()))?;
    require_success("oras cp --help", &cp_help)?;
    let cp_help = combined_output(&cp_help);
    for required in ["--from-oci-layout", "--to-registry-config", "--no-tty"] {
        if !cp_help.contains(required) {
            bail!("ORAS cp does not advertise required option `{required}`");
        }
    }
    if require_ca_file && !cp_help.contains("--to-ca-file") {
        bail!("ORAS cp does not advertise required option `--to-ca-file`");
    }

    let resolve_help = Command::new(oras)
        .args(["resolve", "--help"])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("inspect ORAS resolve support through `{}`", oras.display()))?;
    require_success("oras resolve --help", &resolve_help)?;
    let resolve_help = combined_output(&resolve_help);
    for required in ["--registry-config", "--plain-http"] {
        if !resolve_help.contains(required) {
            bail!("ORAS resolve does not advertise required option `{required}`");
        }
    }

    Ok(version
        .lines()
        .next()
        .unwrap_or(version.trim())
        .trim()
        .to_string())
}

fn resolve_remote(
    oras: &Path,
    registry_config: &Path,
    target: &str,
    options: &OciPushOptions<'_>,
) -> Result<RemoteTag> {
    let mut command = Command::new(oras);
    command.arg("resolve");
    append_resolve_transport(&mut command, registry_config, options)?;
    command.arg(target).stdin(Stdio::null());
    let output = command
        .output()
        .with_context(|| format!("resolve remote OCI tag `{target}` through ORAS"))?;
    if output.status.success() {
        let text = combined_output(&output);
        let digest = text
            .split_whitespace()
            .rev()
            .find_map(|value| OciDigest::parse(value).ok())
            .with_context(|| {
                format!("ORAS resolve returned no canonical SHA-256 digest: {text}")
            })?;
        return Ok(RemoteTag::Digest(digest));
    }
    let diagnostic = combined_output(&output);
    if is_missing_manifest(&diagnostic) {
        return Ok(RemoteTag::Missing);
    }
    bail!(
        "ORAS could not resolve remote OCI tag `{target}` ({}): {}",
        output.status,
        bounded_diagnostic(&diagnostic)
    )
}

fn copy_layout(
    oras: &Path,
    registry_config: &Path,
    layout: &VerifiedLayout,
    target: &str,
    options: &OciPushOptions<'_>,
) -> Result<()> {
    let source = format!("{}:{}", layout.path.display(), layout.tag);
    let mut command = Command::new(oras);
    command.args(["cp", "--from-oci-layout", "--no-tty"]);
    append_copy_transport(&mut command, registry_config, options)?;
    command.args([&source, target]).stdin(Stdio::null());
    let output = command.output().with_context(|| {
        format!("copy verified OCI layout `{source}` to `{target}` through ORAS")
    })?;
    require_success("oras cp", &output)
}

fn append_resolve_transport(
    command: &mut Command,
    registry_config: &Path,
    options: &OciPushOptions<'_>,
) -> Result<()> {
    command.arg("--registry-config").arg(registry_config);
    if options.plain_http {
        command.arg("--plain-http");
    }
    if options.insecure_tls {
        command.arg("--insecure");
    }
    if let Some(ca_file) = options.ca_file {
        command.arg("--ca-file").arg(fs::canonicalize(ca_file)?);
    }
    Ok(())
}

fn append_copy_transport(
    command: &mut Command,
    registry_config: &Path,
    options: &OciPushOptions<'_>,
) -> Result<()> {
    command.arg("--to-registry-config").arg(registry_config);
    if options.plain_http {
        command.arg("--to-plain-http");
    }
    if options.insecure_tls {
        command.arg("--to-insecure");
    }
    if let Some(ca_file) = options.ca_file {
        command.arg("--to-ca-file").arg(fs::canonicalize(ca_file)?);
    }
    Ok(())
}

fn require_success(label: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{label} failed with {}: {}",
        output.status,
        bounded_diagnostic(&combined_output(output))
    )
}

fn combined_output(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    text
}

fn bounded_diagnostic(value: &str) -> String {
    let mut value = value
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '�'
            } else {
                character
            }
        })
        .collect::<String>();
    if value.len() > MAX_DIAGNOSTIC_BYTES {
        let mut end = MAX_DIAGNOSTIC_BYTES;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.push('…');
    }
    value.trim().to_string()
}

fn is_missing_manifest(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    if [
        "manifest unknown",
        "manifest not found",
        "no such manifest",
        "name unknown",
        "response status code 404",
        "404 not found",
    ]
    .iter()
    .any(|needle| value.contains(needle))
    {
        return true;
    }

    let value = value.trim_end();
    value.contains("error response from registry:")
        && value.contains("failed to resolve digest:")
        && value.ends_with(": not found")
}

fn registry_target(destination: &OciReference) -> Result<String> {
    let tag = destination
        .tag
        .as_deref()
        .context("OCI registry target requires a tag")?;
    Ok(format!(
        "{}/{}:{}",
        destination.registry, destination.repository, tag
    ))
}

fn is_loopback_registry(registry: &str) -> bool {
    let host = registry.split_once(':').map_or(registry, |(host, _)| host);
    host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
}

fn encode_base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(first >> 2) as usize] as char);
        output.push(TABLE[(((first & 0b0000_0011) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((second & 0b0000_1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(third & 0b0011_1111) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn push_status_name(status: OciPushStatus) -> &'static str {
    match status {
        OciPushStatus::Pushed => "pushed",
        OciPushStatus::Replaced => "replaced",
        OciPushStatus::AlreadyPresent => "already-present",
    }
}

fn authentication_name(authentication: OciAuthentication) -> &'static str {
    match authentication {
        OciAuthentication::PasswordStdin => "ephemeral password-stdin config",
        OciAuthentication::RegistryConfig => "explicit registry config",
        OciAuthentication::Anonymous => "anonymous",
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use serde_json::Value;

    use super::*;
    use crate::oci_layout;
    use zed_interfaces::paths::MANIFEST_FILE;

    fn write_project(project: &Path) {
        fs::create_dir_all(project).unwrap();
        fs::write(
            project.join(MANIFEST_FILE),
            r#"[package]
org = "acme"
name = "tool"
version = "1.2.3"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/tool"
"#,
        )
        .unwrap();
        fs::write(project.join("lib.txt"), "payload\n").unwrap();
    }

    fn make_layout(workspace: &Path) -> (PathBuf, OciDigest) {
        let project = workspace.join("project");
        write_project(&project);
        let layout = workspace.join("layout");
        let result = oci_layout::write_layout(
            &project,
            "oci://localhost:5000/acme/tool:1.2.3",
            None,
            &layout,
        )
        .unwrap();
        (layout, result.manifest.digest)
    }

    #[cfg(unix)]
    fn fake_oras(workspace: &Path, expected: &OciDigest, remote: Option<&OciDigest>) -> PathBuf {
        let directory = workspace.join("fake-oras");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("expected"), format!("{expected}\n")).unwrap();
        if let Some(remote) = remote {
            fs::write(directory.join("state"), format!("{remote}\n")).unwrap();
        }
        let executable = directory.join("oras");
        fs::write(
            &executable,
            r#"#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$*" >> "$root/log"
case "${1:-}" in
  version)
    echo 'Version: 1.3.2'
    ;;
  cp)
    if [ "${2:-}" = '--help' ]; then
      echo '--from-oci-layout --to-registry-config --to-ca-file --no-tty'
    else
      cp "$root/expected" "$root/state"
      echo 'Copied'
    fi
    ;;
  resolve)
    if [ "${2:-}" = '--help' ]; then
      echo '--registry-config --plain-http --ca-file'
    elif [ -f "$root/state" ]; then
      cat "$root/state"
    else
      echo 'Error response from registry: failed to resolve digest: localhost:5000/acme/tool:1.2.3: not found' >&2
      exit 1
    fi
    ;;
  *)
    echo "unexpected command: $*" >&2
    exit 64
    ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        executable
    }

    fn options<'a>(layout: &'a Path, oras: &'a Path) -> OciPushOptions<'a> {
        OciPushOptions {
            layout,
            destination: "oci://localhost:5000/acme/tool:1.2.3",
            oras,
            username: None,
            password_stdin: false,
            registry_config: None,
            anonymous: true,
            plain_http: true,
            insecure_tls: false,
            ca_file: None,
            allow_tag_replacement: false,
            interactive: false,
            json: true,
        }
    }

    #[test]
    fn recognizes_registry_missing_tags_without_masking_unrelated_errors() {
        assert!(is_missing_manifest(
            "Error response from registry: failed to resolve digest: registry.example/acme/tool:1.2.3: not found
"
        ));
        assert!(is_missing_manifest("MANIFEST_UNKNOWN: manifest unknown"));
        assert!(!is_missing_manifest(
            "open /tmp/registry-config.json: no such file or directory"
        ));
        assert!(!is_missing_manifest(
            "Error response from registry: failed to resolve digest: registry.example/acme/tool:1.2.3: unauthorized"
        ));
        assert!(!is_missing_manifest(
            "dial tcp registry.example:443: connect: network is unreachable"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn pushes_verified_layout_and_re_resolves_exact_digest() {
        let workspace = tempfile::tempdir().unwrap();
        let (layout, digest) = make_layout(workspace.path());
        let oras = fake_oras(workspace.path(), &digest, None);
        let result = execute(&options(&layout, &oras), None).unwrap();

        assert_eq!(result.status, OciPushStatus::Pushed);
        assert_eq!(result.destination.digest.as_ref(), Some(&digest));
        assert_eq!(result.authentication, OciAuthentication::Anonymous);
        let log = fs::read_to_string(workspace.path().join("fake-oras/log")).unwrap();
        assert!(log.contains("cp --from-oci-layout --no-tty"));
        assert!(log.contains("--to-registry-config"));
        assert!(log.contains("localhost:5000/acme/tool:1.2.3"));
        assert!(!log.contains("oci://"));
    }

    #[cfg(unix)]
    #[test]
    fn same_digest_is_idempotent_and_different_digest_requires_consent() {
        let workspace = tempfile::tempdir().unwrap();
        let (layout, digest) = make_layout(workspace.path());
        let oras = fake_oras(workspace.path(), &digest, Some(&digest));
        let result = execute(&options(&layout, &oras), None).unwrap();
        assert_eq!(result.status, OciPushStatus::AlreadyPresent);

        let other = OciDigest::parse(format!("sha256:{}", "f".repeat(64))).unwrap();
        fs::write(
            workspace.path().join("fake-oras/state"),
            format!("{other}\n"),
        )
        .unwrap();
        let error = execute(&options(&layout, &oras), None).unwrap_err();
        assert!(error.to_string().contains("refusing to replace OCI tag"));

        let mut replacement = options(&layout, &oras);
        replacement.allow_tag_replacement = true;
        let result = execute(&replacement, None).unwrap();
        assert_eq!(result.status, OciPushStatus::Replaced);
    }

    #[test]
    fn layout_verification_rejects_tampered_blob_before_transport() {
        let workspace = tempfile::tempdir().unwrap();
        let (layout, _) = make_layout(workspace.path());
        let index: Value =
            serde_json::from_slice(&fs::read(layout.join(OCI_INDEX_FILE)).unwrap()).unwrap();
        let manifest_digest = index["manifests"][0]["digest"]
            .as_str()
            .unwrap()
            .strip_prefix("sha256:")
            .unwrap();
        fs::write(
            layout.join("blobs/sha256").join(manifest_digest),
            "tampered",
        )
        .unwrap();

        let destination = parse_destination("oci://localhost:5000/acme/tool:1.2.3").unwrap();
        let error = verify_layout(&layout, &destination).unwrap_err();
        assert!(
            error.to_string().contains("size drift") || error.to_string().contains("digest drift")
        );
    }

    #[test]
    fn ephemeral_password_config_is_mode_0600_and_docker_compatible() {
        assert_eq!(encode_base64(b"user:pass"), "dXNlcjpwYXNz");
        let workspace = tempfile::tempdir().unwrap();
        let layout = workspace.path().join("unused-layout");
        let oras = workspace.path().join("unused-oras");
        let options = OciPushOptions {
            layout: &layout,
            destination: "oci://ghcr.io/acme/tool:1.2.3",
            oras: &oras,
            username: Some("user"),
            password_stdin: true,
            registry_config: None,
            anonymous: false,
            plain_http: false,
            insecure_tls: false,
            ca_file: None,
            allow_tag_replacement: false,
            interactive: false,
            json: true,
        };
        let destination = parse_destination(options.destination).unwrap();
        let prepared = prepare_registry_config(&options, &destination, Some("pass")).unwrap();
        let config: Value = serde_json::from_slice(&fs::read(&prepared.path).unwrap()).unwrap();
        assert_eq!(config["auths"]["ghcr.io"]["auth"], "dXNlcjpwYXNz");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&prepared.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn runtime_auth_validation_rejects_missing_partial_and_conflicting_modes() {
        let workspace = tempfile::tempdir().unwrap();
        let layout = workspace.path().join("unused-layout");
        let oras = workspace.path().join("unused-oras");
        let destination = parse_destination("oci://ghcr.io/acme/tool:1.2.3").unwrap();
        let mut options = OciPushOptions {
            layout: &layout,
            destination: "oci://ghcr.io/acme/tool:1.2.3",
            oras: &oras,
            username: None,
            password_stdin: false,
            registry_config: None,
            anonymous: false,
            plain_http: false,
            insecure_tls: false,
            ca_file: None,
            allow_tag_replacement: false,
            interactive: false,
            json: true,
        };

        let missing = prepare_registry_config(&options, &destination, None)
            .err()
            .unwrap();
        assert!(
            missing
                .to_string()
                .contains("choose exactly one OCI authentication mode")
        );

        options.username = Some("user");
        let partial = prepare_registry_config(&options, &destination, None)
            .err()
            .unwrap();
        assert!(partial.to_string().contains("did not yield a password"));

        options.username = None;
        options.password_stdin = true;
        let partial = prepare_registry_config(&options, &destination, Some("pass"))
            .err()
            .unwrap();
        assert!(partial.to_string().contains("--username is required"));

        options.username = Some("user");
        options.anonymous = true;
        let conflicting = prepare_registry_config(&options, &destination, Some("pass"))
            .err()
            .unwrap();
        assert!(
            conflicting
                .to_string()
                .contains("choose exactly one OCI authentication mode")
        );
    }

    #[test]
    fn runtime_transport_validation_rejects_conflicting_security_modes() {
        let workspace = tempfile::tempdir().unwrap();
        let ca_file = workspace.path().join("registry-ca.pem");
        fs::write(&ca_file, "test CA bytes").unwrap();
        let layout = workspace.path().join("unused-layout");
        let oras = workspace.path().join("unused-oras");
        let destination = parse_destination("oci://127.0.0.1:5000/acme/tool:1.2.3").unwrap();
        let mut options = OciPushOptions {
            layout: &layout,
            destination: "oci://127.0.0.1:5000/acme/tool:1.2.3",
            oras: &oras,
            username: None,
            password_stdin: false,
            registry_config: None,
            anonymous: true,
            plain_http: true,
            insecure_tls: true,
            ca_file: None,
            allow_tag_replacement: false,
            interactive: false,
            json: true,
        };

        let plain_and_insecure = validate_transport_options(&options, &destination).unwrap_err();
        assert!(
            plain_and_insecure
                .to_string()
                .contains("--plain-http cannot")
        );

        options.insecure_tls = false;
        options.ca_file = Some(&ca_file);
        let plain_and_ca = validate_transport_options(&options, &destination).unwrap_err();
        assert!(plain_and_ca.to_string().contains("--plain-http cannot"));

        options.plain_http = false;
        options.insecure_tls = true;
        let insecure_and_ca = validate_transport_options(&options, &destination).unwrap_err();
        assert!(
            insecure_and_ca
                .to_string()
                .contains("--insecure-tls cannot")
        );
    }

    #[test]
    fn plain_http_fails_closed_for_non_loopback_registry() {
        let workspace = tempfile::tempdir().unwrap();
        let options = OciPushOptions {
            layout: workspace.path(),
            destination: "oci://ghcr.io/acme/tool:1.2.3",
            oras: Path::new("oras"),
            username: None,
            password_stdin: false,
            registry_config: None,
            anonymous: true,
            plain_http: true,
            insecure_tls: false,
            ca_file: None,
            allow_tag_replacement: false,
            interactive: false,
            json: true,
        };
        let destination = parse_destination(options.destination).unwrap();
        assert!(validate_transport_options(&options, &destination).is_err());
    }
}
