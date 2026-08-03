use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::{
    ArtifactFormat, OCI_IMAGE_MANIFEST_MEDIA_TYPE, OciDescriptor, OciDigest, OciPackageIdentity,
    OciReference, ZED_OCI_CONFIG_MEDIA_TYPE_V1,
};

use crate::config::read_manifest;
use crate::oci::{self, OciPlannedBlob, OciPlannedBlobKind, OciPublishPlan};
use crate::pack::{self, PackagedTarget};

pub const OCI_LAYOUT_RESULT_SCHEMA_V1: &str = "zed.oci-layout-result/v1";
const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const OCI_LAYOUT_VERSION: &str = "1.0.0";
const OCI_LAYOUT_FILE: &str = "oci-layout";
const OCI_INDEX_FILE: &str = "index.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OciLayoutResult {
    pub schema: String,
    pub path: String,
    pub requested_destination: OciReference,
    pub resolved_reference: OciReference,
    pub manifest: OciDescriptor,
    pub blob_count: usize,
    pub total_blob_bytes: u64,
}

#[derive(Debug)]
struct MaterializedBlob {
    planned: OciPlannedBlob,
    bytes: Vec<u8>,
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
    annotations: &'a std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciImageIndex {
    schema_version: u32,
    media_type: &'static str,
    manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciLayoutVersion {
    image_layout_version: &'static str,
}

pub fn materialize(
    project: &Path,
    destination: &str,
    target: Option<&str>,
    out: &Path,
    json: bool,
) -> Result<()> {
    let result = write_layout(project, destination, target, out)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("OCI image layout");
        println!("path: {}", result.path);
        println!("requested: {}", result.requested_destination);
        println!("resolved: {}", result.resolved_reference);
        println!("manifest: {}", result.manifest.digest);
        println!("blobs: {}", result.blob_count);
        println!("blob bytes: {}", result.total_blob_bytes);
        println!("credentials: not read");
        println!("network/uploads: not performed");
    }
    Ok(())
}

pub fn write_layout(
    project: &Path,
    destination: &str,
    target: Option<&str>,
    out: &Path,
) -> Result<OciLayoutResult> {
    if out.exists() {
        bail!(
            "refusing to replace existing OCI layout output `{}`",
            out.display()
        );
    }

    let plan = oci::build_plan(project, destination, target)?;
    let blobs = reconstruct_blobs(project, &plan)?;
    let parent = out
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create OCI layout parent {}", parent.display()))?;

    let staging = tempfile::Builder::new()
        .prefix(".zed-oci-layout-")
        .tempdir_in(parent)
        .with_context(|| format!("create OCI layout staging directory in {}", parent.display()))?;
    let blob_dir = staging.path().join("blobs/sha256");
    fs::create_dir_all(&blob_dir).context("create OCI blob directory")?;

    let mut seen = BTreeSet::new();
    let mut total_blob_bytes = 0u64;
    for blob in &blobs {
        let encoded = blob.planned.digest.encoded().ok_or_else(|| {
            anyhow::anyhow!(
                "planned OCI blob `{}` is not a canonical SHA-256 digest",
                blob.planned.digest
            )
        })?;
        if !seen.insert(encoded.to_string()) {
            bail!(
                "planned OCI blob digest `{}` appears more than once",
                blob.planned.digest
            );
        }
        fs::write(blob_dir.join(encoded), &blob.bytes)
            .with_context(|| format!("write OCI blob {}", blob.planned.digest))?;
        total_blob_bytes = total_blob_bytes
            .checked_add(blob.planned.size)
            .ok_or_else(|| anyhow::anyhow!("OCI layout blob byte count overflow"))?;
    }

    let layout_bytes = serde_json::to_vec(&OciLayoutVersion {
        image_layout_version: OCI_LAYOUT_VERSION,
    })?;
    fs::write(staging.path().join(OCI_LAYOUT_FILE), layout_bytes)
        .context("write OCI layout version file")?;

    let mut index_manifest = plan.adapter.manifest.clone();
    let tag = plan
        .requested_destination
        .tag
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("planned OCI layout is missing its requested tag"))?;
    index_manifest.annotations.insert(
        "org.opencontainers.image.ref.name".to_string(),
        tag.clone(),
    );
    index_manifest
        .validate("OCI image index manifest")
        .map_err(|error| anyhow::anyhow!(error))?;
    let index_bytes = serde_json::to_vec(&OciImageIndex {
        schema_version: 2,
        media_type: OCI_IMAGE_INDEX_MEDIA_TYPE,
        manifests: vec![index_manifest],
    })?;
    fs::write(staging.path().join(OCI_INDEX_FILE), index_bytes)
        .context("write OCI image index")?;

    if out.exists() {
        bail!(
            "refusing to replace OCI layout output `{}` created while materializing",
            out.display()
        );
    }
    fs::rename(staging.path(), out).with_context(|| {
        format!(
            "atomically publish OCI layout from {} to {}",
            staging.path().display(),
            out.display()
        )
    })?;

    Ok(OciLayoutResult {
        schema: OCI_LAYOUT_RESULT_SCHEMA_V1.to_string(),
        path: out.display().to_string(),
        requested_destination: plan.requested_destination,
        resolved_reference: plan.resolved_reference,
        manifest: plan.adapter.manifest,
        blob_count: blobs.len(),
        total_blob_bytes,
    })
}

fn reconstruct_blobs(project: &Path, plan: &OciPublishPlan) -> Result<Vec<MaterializedBlob>> {
    let root_manifest = read_manifest(project)?;
    let temporary_output = tempfile::tempdir().context("create OCI layout packing directory")?;
    let packages = pack::pack_all(project, &root_manifest, Some(temporary_output.path()))?;
    let selected = select_packaged_target(packages, plan.package.target.as_deref())?;

    let package_bytes = fs::read(&selected.packed.path)
        .with_context(|| format!("read packed OCI payload {}", selected.packed.path.display()))?;
    let manifest_bytes = if selected.target.is_some() {
        selected.manifest.to_toml_string()?.into_bytes()
    } else {
        fs::read(project.join(MANIFEST_FILE)).context("read source .zpkg.toml")?
    };
    let lock_bytes = planned_blob(plan, OciPlannedBlobKind::Lockfile)
        .map(|_| fs::read(project.join(LOCKFILE_FILE)).context("read source .zpkg.lock"))
        .transpose()?;

    let package = planned_blob_required(plan, OciPlannedBlobKind::Package)?;
    let manifest = planned_blob_required(plan, OciPlannedBlobKind::Manifest)?;
    let lock = planned_blob(plan, OciPlannedBlobKind::Lockfile);
    let config_bytes = serde_json::to_vec(&ZedOciConfig {
        schema: "zed.oci-config/v1",
        package: &plan.package,
        repository: &selected.manifest.package.repository.url,
        vcs_tag: selected.manifest.vcs_tag(),
        artifact: ZedOciArtifact {
            format: selected.packed.format,
            digest: &package.digest,
            size: selected.packed.size,
        },
        manifest_digest: &manifest.digest,
        lock_digest: lock.map(|blob| &blob.digest),
    })?;
    let oci_manifest_bytes = serde_json::to_vec(&OciImageManifest {
        schema_version: 2,
        media_type: OCI_IMAGE_MANIFEST_MEDIA_TYPE,
        artifact_type: ZED_OCI_CONFIG_MEDIA_TYPE_V1,
        config: &plan.adapter.config,
        layers: plan
            .adapter
            .layers
            .iter()
            .map(|layer| &layer.descriptor)
            .collect(),
        annotations: &plan.adapter.annotations,
    })?;

    let mut materialized = vec![
        materialized_blob(plan, OciPlannedBlobKind::Config, config_bytes)?,
        materialized_blob(plan, OciPlannedBlobKind::Package, package_bytes)?,
        materialized_blob(plan, OciPlannedBlobKind::Manifest, manifest_bytes)?,
    ];
    if let Some(lock_bytes) = lock_bytes {
        materialized.push(materialized_blob(
            plan,
            OciPlannedBlobKind::Lockfile,
            lock_bytes,
        )?);
    }
    materialized.push(materialized_blob(
        plan,
        OciPlannedBlobKind::OciManifest,
        oci_manifest_bytes,
    )?);
    materialized.sort_by_key(|blob| blob.planned.kind);

    if materialized.len() != plan.blobs.len() {
        bail!(
            "OCI layout reconstructed {} blobs but the immutable plan declares {}",
            materialized.len(),
            plan.blobs.len()
        );
    }
    Ok(materialized)
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
                "packed release set did not contain target `{}` while materializing the OCI layout",
                target.unwrap_or("repository")
            )
        })
}

fn materialized_blob(
    plan: &OciPublishPlan,
    kind: OciPlannedBlobKind,
    bytes: Vec<u8>,
) -> Result<MaterializedBlob> {
    let planned = planned_blob_required(plan, kind)?.clone();
    let actual_digest = digest_bytes(&bytes)?;
    let actual_size = bytes.len() as u64;
    if actual_digest != planned.digest || actual_size != planned.size {
        bail!(
            "OCI `{}` blob drifted after planning: expected {} ({} bytes), reconstructed {} ({} bytes)",
            blob_kind_name(kind),
            planned.digest,
            planned.size,
            actual_digest,
            actual_size
        );
    }
    Ok(MaterializedBlob { planned, bytes })
}

fn planned_blob(plan: &OciPublishPlan, kind: OciPlannedBlobKind) -> Option<&OciPlannedBlob> {
    plan.blobs.iter().find(|blob| blob.kind == kind)
}

fn planned_blob_required(
    plan: &OciPublishPlan,
    kind: OciPlannedBlobKind,
) -> Result<&OciPlannedBlob> {
    planned_blob(plan, kind).ok_or_else(|| {
        anyhow::anyhow!(
            "immutable OCI plan is missing its `{}` blob",
            blob_kind_name(kind)
        )
    })
}

fn digest_bytes(bytes: &[u8]) -> Result<OciDigest> {
    OciDigest::parse(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
        .map_err(|error| anyhow::anyhow!(error))
}

fn blob_kind_name(kind: OciPlannedBlobKind) -> &'static str {
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
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use serde_json::Value;

    use super::*;

    fn write_manifest(project: &Path, extra: &str) {
        fs::write(
            project.join(MANIFEST_FILE),
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
            ),
        )
        .unwrap();
        fs::write(project.join("lib.txt"), "payload\n").unwrap();
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, current: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let mut entries = fs::read_dir(current)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &path, output);
                } else {
                    output.insert(
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }

        let mut output = BTreeMap::new();
        visit(root, root, &mut output);
        output
    }

    #[test]
    fn writes_deterministic_standard_layout_with_verified_blob_names() {
        let workspace = tempfile::tempdir().unwrap();
        let project = workspace.path().join("project");
        fs::create_dir(&project).unwrap();
        write_manifest(&project, "");
        let first = workspace.path().join("layout-a");
        let second = workspace.path().join("layout-b");

        let result = write_layout(
            &project,
            "oci://ghcr.io/acme/tool:1.2.3",
            None,
            &first,
        )
        .unwrap();
        write_layout(
            &project,
            "oci://ghcr.io/acme/tool:1.2.3",
            None,
            &second,
        )
        .unwrap();

        assert_eq!(snapshot(&first), snapshot(&second));
        assert_eq!(result.blob_count, 4);
        assert!(!project.join(".zed/pack").exists());

        let layout: Value =
            serde_json::from_slice(&fs::read(first.join(OCI_LAYOUT_FILE)).unwrap()).unwrap();
        assert_eq!(layout["imageLayoutVersion"], OCI_LAYOUT_VERSION);
        let index: Value =
            serde_json::from_slice(&fs::read(first.join(OCI_INDEX_FILE)).unwrap()).unwrap();
        assert_eq!(index["schemaVersion"], 2);
        assert_eq!(index["mediaType"], OCI_IMAGE_INDEX_MEDIA_TYPE);
        assert_eq!(
            index["manifests"][0]["digest"],
            result.manifest.digest.as_str()
        );
        assert_eq!(
            index["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            "1.2.3"
        );

        for entry in fs::read_dir(first.join("blobs/sha256")).unwrap() {
            let entry = entry.unwrap();
            let bytes = fs::read(entry.path()).unwrap();
            assert_eq!(
                entry.file_name().to_string_lossy(),
                hex::encode(Sha256::digest(&bytes))
            );
        }
    }

    #[test]
    fn refuses_to_replace_an_existing_output() {
        let workspace = tempfile::tempdir().unwrap();
        let project = workspace.path().join("project");
        fs::create_dir(&project).unwrap();
        write_manifest(&project, "");
        let output = workspace.path().join("layout");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep.txt"), "keep\n").unwrap();

        assert!(
            write_layout(
                &project,
                "oci://ghcr.io/acme/tool:1.2.3",
                None,
                &output,
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(output.join("keep.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn writes_one_selected_polyglot_target() {
        let workspace = tempfile::tempdir().unwrap();
        let project = workspace.path().join("project");
        fs::create_dir_all(project.join("clients/rust")).unwrap();
        fs::write(
            project.join("clients/rust/lib.rs"),
            "pub fn answer() -> u8 { 42 }\n",
        )
        .unwrap();
        write_manifest(
            &project,
            r#"[targets.rust]
dir = "clients/rust"
"#,
        );
        let output = workspace.path().join("layout");

        let result = write_layout(
            &project,
            "oci://ghcr.io/acme/tool-rust:1.2.3",
            Some("rust"),
            &output,
        )
        .unwrap();
        assert!(result.resolved_reference.is_immutable());
        assert_eq!(result.blob_count, 4);
    }
}
