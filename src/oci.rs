use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::version::Requirement;
use zed_interfaces::{
    ArtifactFormat, Lockfile, Manifest, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciAdapterRecord,
    OciDescriptor, OciDigest, OciLayer, OciLayerKind, OciPackageIdentity, OciReference,
    ZED_OCI_CONFIG_MEDIA_TYPE_V1,
};

use crate::config::read_manifest;
use crate::pack::{self, PackagedTarget};
use crate::store::require_sha256;

pub const OCI_PUBLISH_PLAN_SCHEMA_V1: &str = "zed.oci-publish-plan/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OciPublishPlan {
    pub schema: String,
    pub requested_destination: OciReference,
    pub resolved_reference: OciReference,
    pub package: OciPackageIdentity,
    pub adapter: OciAdapterRecord,
    pub blobs: Vec<OciPlannedBlob>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OciPlannedBlob {
    pub kind: OciPlannedBlobKind,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: OciDigest,
    pub size: u64,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OciPlannedBlobKind {
    Config,
    Package,
    Manifest,
    Lockfile,
    OciManifest,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ZedOciConfig<'a> {
    schema: &'static str,
    package: &'a OciPackageIdentity,
    repository: &'a str,
    vcs_tag: String,
    artifact: ZedOciArtifact<'a>,
    manifest_digest: &'a OciDigest,
    #[serde(skip_serializing_if = "Option::is_none")]
    lock_digest: Option<&'a OciDigest>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ZedOciArtifact<'a> {
    format: ArtifactFormat,
    digest: &'a OciDigest,
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

pub fn plan(project: &Path, destination: &str, target: Option<&str>, json: bool) -> Result<()> {
    let plan = build_plan(project, destination, target)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print_human(&plan);
    }
    Ok(())
}

pub fn build_plan(
    project: &Path,
    destination: &str,
    requested_target: Option<&str>,
) -> Result<OciPublishPlan> {
    let root_manifest = read_manifest(project)?;
    let target = resolve_target(&root_manifest, requested_target)?;
    let requested_destination = OciReference::parse(destination)
        .map_err(|error| anyhow::anyhow!("invalid OCI destination: {error}"))?;
    if requested_destination.digest.is_some() {
        bail!(
            "OCI planning accepts a tag destination, not a preselected digest; the exact digest is derived from the planned bytes"
        );
    }
    let tag = requested_destination.tag.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "OCI planning requires an explicit immutable-release tag, for example `oci://ghcr.io/acme/tool:1.2.3`"
        )
    })?;

    let lock = load_and_validate_lock(project, &root_manifest)?;
    let temporary_output = tempfile::tempdir().context("create OCI planning directory")?;
    let packages = pack::pack_all(project, &root_manifest, Some(temporary_output.path()))?;
    let selected = select_packaged_target(packages, target.as_deref())?;
    let package = OciPackageIdentity {
        org: selected.manifest.package.org.clone(),
        name: selected.manifest.package.name.clone(),
        version: selected.manifest.package.version.clone(),
        target: target.clone(),
    };
    package
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid OCI package identity: {error}"))?;
    if tag != package.version {
        bail!(
            "OCI destination tag `{tag}` must equal package version `{}` in contract v1",
            package.version
        );
    }

    let package_digest = sha256_digest_from_hex(&selected.packed.sha256)?;
    let package_layer = OciLayer {
        kind: OciLayerKind::PackageTarGz,
        descriptor: OciDescriptor {
            media_type: OciLayerKind::PackageTarGz.media_type().to_string(),
            digest: package_digest.clone(),
            size: selected.packed.size,
            annotations: BTreeMap::new(),
        },
        platform: None,
    };

    let manifest_bytes = selected_manifest_bytes(project, &root_manifest, &selected)?;
    let manifest_layer =
        layer_from_bytes(OciLayerKind::Manifest, &manifest_bytes, BTreeMap::new())?;

    let lock_layer = lock
        .as_ref()
        .map(|lock| layer_from_bytes(OciLayerKind::Lockfile, &lock.bytes, BTreeMap::new()))
        .transpose()?;

    let config_bytes = serde_json::to_vec(&ZedOciConfig {
        schema: "zed.oci-config/v1",
        package: &package,
        repository: &selected.manifest.package.repository.url,
        vcs_tag: selected.manifest.vcs_tag(),
        artifact: ZedOciArtifact {
            format: selected.packed.format,
            digest: &package_digest,
            size: selected.packed.size,
        },
        manifest_digest: &manifest_layer.descriptor.digest,
        lock_digest: lock_layer.as_ref().map(|layer| &layer.descriptor.digest),
    })?;
    let config =
        descriptor_from_bytes(ZED_OCI_CONFIG_MEDIA_TYPE_V1, &config_bytes, BTreeMap::new())?;

    let mut annotations = BTreeMap::from([
        (
            "org.opencontainers.image.source".to_string(),
            selected.manifest.package.repository.url.clone(),
        ),
        (
            "org.opencontainers.image.title".to_string(),
            format!("{}/{}", package.org, package.name),
        ),
        (
            "org.opencontainers.image.version".to_string(),
            package.version.clone(),
        ),
    ]);
    if let Some(target) = &package.target {
        annotations.insert("tech.zpkg.target".to_string(), target.clone());
    }

    let mut layers = vec![package_layer, manifest_layer];
    if let Some(lock_layer) = lock_layer {
        layers.push(lock_layer);
    }
    layers.sort_by_key(|layer| layer.kind);

    let manifest_bytes = serde_json::to_vec(&OciImageManifest {
        schema_version: 2,
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE,
        artifact_type: ZED_OCI_CONFIG_MEDIA_TYPE_V1,
        config: &config,
        layers: layers.iter().map(|layer| &layer.descriptor).collect(),
        annotations: &annotations,
    })?;
    let manifest_descriptor = descriptor_from_bytes(
        OCI_IMAGE_MANIFEST_MEDIA_TYPE,
        &manifest_bytes,
        BTreeMap::new(),
    )?;

    let mut resolved_reference = requested_destination.clone();
    resolved_reference.digest = Some(manifest_descriptor.digest.clone());
    let mut adapter = OciAdapterRecord::new(
        package.clone(),
        resolved_reference.clone(),
        manifest_descriptor.clone(),
        config.clone(),
        layers.clone(),
    );
    adapter.annotations = annotations;
    adapter
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid finalized OCI plan: {error}"))?;

    let mut blobs = vec![
        OciPlannedBlob {
            kind: OciPlannedBlobKind::Config,
            media_type: config.media_type.clone(),
            digest: config.digest.clone(),
            size: config.size,
            source: "deterministic zed OCI config JSON".to_string(),
        },
        OciPlannedBlob {
            kind: OciPlannedBlobKind::Package,
            media_type: layers[0].descriptor.media_type.clone(),
            digest: package_digest,
            size: selected.packed.size,
            source: "deterministic temporary `zed pack` output".to_string(),
        },
        OciPlannedBlob {
            kind: OciPlannedBlobKind::Manifest,
            media_type: OciLayerKind::Manifest.media_type().to_string(),
            digest: layers
                .iter()
                .find(|layer| layer.kind == OciLayerKind::Manifest)
                .expect("manifest layer exists")
                .descriptor
                .digest
                .clone(),
            size: manifest_bytes_len(&layers, OciLayerKind::Manifest),
            source: if target.is_some() {
                "derived single-target .zpkg.toml".to_string()
            } else {
                MANIFEST_FILE.to_string()
            },
        },
    ];
    if let Some(lock) = lock {
        let layer = layers
            .iter()
            .find(|layer| layer.kind == OciLayerKind::Lockfile)
            .expect("lock layer exists");
        blobs.push(OciPlannedBlob {
            kind: OciPlannedBlobKind::Lockfile,
            media_type: layer.descriptor.media_type.clone(),
            digest: layer.descriptor.digest.clone(),
            size: layer.descriptor.size,
            source: lock.path,
        });
    }
    blobs.push(OciPlannedBlob {
        kind: OciPlannedBlobKind::OciManifest,
        media_type: manifest_descriptor.media_type.clone(),
        digest: manifest_descriptor.digest.clone(),
        size: manifest_descriptor.size,
        source: "deterministic OCI image manifest JSON".to_string(),
    });
    blobs.sort_by_key(|blob| blob.kind);

    Ok(OciPublishPlan {
        schema: OCI_PUBLISH_PLAN_SCHEMA_V1.to_string(),
        requested_destination,
        resolved_reference,
        package,
        adapter,
        blobs,
    })
}

#[derive(Debug)]
struct ValidatedLock {
    path: String,
    bytes: Vec<u8>,
}

fn load_and_validate_lock(project: &Path, manifest: &Manifest) -> Result<Option<ValidatedLock>> {
    let path = project.join(LOCKFILE_FILE);
    if !path.exists() {
        if manifest.dependencies.is_empty() && manifest.build_dependencies.is_empty() {
            return Ok(None);
        }
        bail!(
            "{LOCKFILE_FILE} is required for OCI planning when dependencies or build-dependencies are declared"
        );
    }

    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("{} must be UTF-8 TOML", path.display()))?;
    let lock = Lockfile::parse(text).with_context(|| format!("parse {}", path.display()))?;
    validate_locked_packages(&lock)?;

    for (key, requirement) in manifest
        .dependencies
        .iter()
        .chain(manifest.build_dependencies.iter())
    {
        let (org, name) = split_dependency_key(key)?;
        let locked = lock.find(org, name).ok_or_else(|| {
            anyhow::anyhow!(
                "frozen OCI plan requires `{key}` in {LOCKFILE_FILE}; run `zed install` to refresh the lock"
            )
        })?;
        if !Requirement::parse(requirement).matches(&locked.version) {
            bail!(
                "frozen OCI plan rejected lock drift: `{key}` requires `{requirement}` but {LOCKFILE_FILE} pins `{}`",
                locked.version
            );
        }
    }

    Ok(Some(ValidatedLock {
        path: LOCKFILE_FILE.to_string(),
        bytes,
    }))
}

fn validate_locked_packages(lock: &Lockfile) -> Result<()> {
    let mut identities = BTreeSet::new();
    for package in &lock.packages {
        let identity = format!("{}/{}", package.org, package.name);
        if !identities.insert(identity.clone()) {
            bail!("{LOCKFILE_FILE} contains duplicate package `{identity}`");
        }
        require_sha256(&package.sha256)
            .with_context(|| format!("{LOCKFILE_FILE} package `{identity}`"))?;
        if package.size == 0 {
            bail!("{LOCKFILE_FILE} package `{identity}` has zero artifact size");
        }
        if package.vcs_tag.trim().is_empty() || package.source.trim().is_empty() {
            bail!("{LOCKFILE_FILE} package `{identity}` is missing source or VCS tag provenance");
        }
    }
    Ok(())
}

fn resolve_target(manifest: &Manifest, requested: Option<&str>) -> Result<Option<String>> {
    if !manifest.is_polyglot() {
        if let Some(requested) = requested {
            bail!(
                "--target `{requested}` is only valid for a polyglot package; this manifest publishes one repository artifact"
            );
        }
        return Ok(None);
    }

    let requested = requested.ok_or_else(|| {
        let available = manifest
            .targets
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!("polyglot OCI planning requires --target; available targets: {available}")
    })?;
    let target = manifest.resolve_target_key(requested).ok_or_else(|| {
        let available = manifest.targets.keys().cloned().collect::<Vec<_>>().join(", ");
        anyhow::anyhow!(
            "manifest publishes no OCI target matching `{requested}`; available targets: {available}"
        )
    })?;
    Ok(Some(target.to_string()))
}

fn select_packaged_target(
    packages: Vec<PackagedTarget>,
    target: Option<&str>,
) -> Result<PackagedTarget> {
    packages
        .into_iter()
        .find(|package| package.target.as_deref() == target)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "packed release set did not contain target `{}`",
                target.unwrap_or("repository")
            )
        })
}

fn selected_manifest_bytes(
    project: &Path,
    root_manifest: &Manifest,
    selected: &PackagedTarget,
) -> Result<Vec<u8>> {
    if selected.target.is_some() {
        return Ok(selected.manifest.to_toml_string()?.into_bytes());
    }
    if root_manifest != &selected.manifest {
        bail!("single-package OCI planning changed the source manifest unexpectedly");
    }
    fs::read(project.join(MANIFEST_FILE)).context("read source .zpkg.toml")
}

fn descriptor_from_bytes(
    media_type: &str,
    bytes: &[u8],
    annotations: BTreeMap<String, String>,
) -> Result<OciDescriptor> {
    if bytes.is_empty() {
        bail!("cannot create an OCI descriptor for empty `{media_type}` content");
    }
    Ok(OciDescriptor {
        media_type: media_type.to_string(),
        digest: sha256_digest(bytes)?,
        size: bytes.len() as u64,
        annotations,
    })
}

fn layer_from_bytes(
    kind: OciLayerKind,
    bytes: &[u8],
    annotations: BTreeMap<String, String>,
) -> Result<OciLayer> {
    Ok(OciLayer {
        kind,
        descriptor: descriptor_from_bytes(kind.media_type(), bytes, annotations)?,
        platform: None,
    })
}

fn sha256_digest(bytes: &[u8]) -> Result<OciDigest> {
    OciDigest::parse(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
        .map_err(|error| anyhow::anyhow!(error))
}

fn sha256_digest_from_hex(hex_digest: &str) -> Result<OciDigest> {
    require_sha256(hex_digest)?;
    OciDigest::parse(format!("sha256:{hex_digest}")).map_err(|error| anyhow::anyhow!(error))
}

fn split_dependency_key(key: &str) -> Result<(&str, &str)> {
    let Some((org, name)) = key.split_once('/') else {
        bail!("invalid dependency key `{key}`; expected org/name");
    };
    if org.is_empty() || name.is_empty() || name.contains('/') {
        bail!("invalid dependency key `{key}`; expected org/name");
    }
    Ok((org, name))
}

fn manifest_bytes_len(layers: &[OciLayer], kind: OciLayerKind) -> u64 {
    layers
        .iter()
        .find(|layer| layer.kind == kind)
        .map(|layer| layer.descriptor.size)
        .unwrap_or_default()
}

fn print_human(plan: &OciPublishPlan) {
    println!("OCI publication plan");
    println!(
        "package: {}/{}@{}{}",
        plan.package.org,
        plan.package.name,
        plan.package.version,
        plan.package
            .target
            .as_deref()
            .map(|target| format!(" target={target}"))
            .unwrap_or_default()
    );
    println!("destination: {}", plan.requested_destination);
    println!("resolved: {}", plan.resolved_reference);
    println!("blobs:");
    for blob in &plan.blobs {
        println!(
            "  {:<12} {} {} bytes {}",
            planned_blob_kind(blob.kind),
            blob.digest,
            blob.size,
            blob.media_type
        );
    }
    println!("credentials: not read");
    println!("network/uploads: not performed");
}

fn planned_blob_kind(kind: OciPlannedBlobKind) -> &'static str {
    match kind {
        OciPlannedBlobKind::Config => "config",
        OciPlannedBlobKind::Package => "package",
        OciPlannedBlobKind::Manifest => "manifest",
        OciPlannedBlobKind::Lockfile => "lockfile",
        OciPlannedBlobKind::OciManifest => "oci-manifest",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn write_manifest(project: &Path, body: &str) {
        fs::write(project.join(MANIFEST_FILE), body).unwrap();
        fs::write(project.join("lib.txt"), "payload\n").unwrap();
    }

    fn simple_manifest(extra: &str) -> String {
        format!(
            r#"[package]
org = "acme"
name = "tool"
version = "1.2.3"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/tool"

{extra}
"#
        )
    }

    #[test]
    fn plan_derives_exact_manifest_digest_without_network_or_persistent_output() {
        let project = tempfile::tempdir().unwrap();
        write_manifest(project.path(), &simple_manifest(""));

        let plan = build_plan(project.path(), "oci://ghcr.io/acme/tool:1.2.3", None).unwrap();

        assert_eq!(plan.package.name, "tool");
        assert!(plan.resolved_reference.is_immutable());
        plan.adapter.validate().unwrap();
        assert!(
            plan.blobs
                .iter()
                .any(|blob| { blob.kind == OciPlannedBlobKind::Package && blob.size > 0 })
        );
        assert!(!project.path().join(".zed/pack").exists());
    }

    #[test]
    fn plan_rejects_tag_drift_and_preselected_digest() {
        let project = tempfile::tempdir().unwrap();
        write_manifest(project.path(), &simple_manifest(""));

        assert!(build_plan(project.path(), "oci://ghcr.io/acme/tool:latest", None).is_err());
        assert!(
            build_plan(
                project.path(),
                &format!("oci://ghcr.io/acme/tool:1.2.3@sha256:{SHA_A}"),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn frozen_plan_requires_complete_matching_lock_provenance() {
        let project = tempfile::tempdir().unwrap();
        write_manifest(
            project.path(),
            &simple_manifest(
                r#"[dependencies]
"acme/dep" = "^1"
"#,
            ),
        );
        assert!(build_plan(project.path(), "oci://ghcr.io/acme/tool:1.2.3", None,).is_err());

        fs::write(
            project.path().join(LOCKFILE_FILE),
            format!(
                r#"version = 1

[[package]]
org = "acme"
name = "dep"
version = "2.0.0"
sha256 = "{SHA_A}"
size = 10
vcs_tag = "v2.0.0"
source = "https://registry.zpkg.net"
"#
            ),
        )
        .unwrap();
        assert!(build_plan(project.path(), "oci://ghcr.io/acme/tool:1.2.3", None,).is_err());
    }

    #[test]
    fn polyglot_plan_requires_and_re_roots_one_target() {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("clients/rust")).unwrap();
        fs::write(
            project.path().join("clients/rust/lib.rs"),
            "pub fn answer() -> u8 { 42 }\n",
        )
        .unwrap();
        write_manifest(
            project.path(),
            &simple_manifest(
                r#"[targets.rust]
dir = "clients/rust"
"#,
            ),
        );

        assert!(build_plan(project.path(), "oci://ghcr.io/acme/tool-rust:1.2.3", None,).is_err());
        let plan = build_plan(
            project.path(),
            "oci://ghcr.io/acme/tool-rust:1.2.3",
            Some("rust"),
        )
        .unwrap();
        assert_eq!(plan.package.name, "tool-rust");
        assert_eq!(plan.package.target.as_deref(), Some("rust"));
    }
}
