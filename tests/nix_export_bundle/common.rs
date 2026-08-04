use std::collections::BTreeMap;

use flate2::{Compression, GzBuilder};
use serde_json::json;
use sha2::{Digest, Sha256};
use zed_cli::nix_export_plan::{
    NIX_EXPORT_PLAN_SCHEMA_V1, NixExportPlan, PlannedDependency, PlannedPackageClass,
    PlannedZedArtifact, ResolvedNixIntent,
};
use zed_interfaces::{
    NixBuilderNetwork, NixExportMode, NixInteropArtifact, NixPackageIdentity, NixPolicyEvidence,
    NixPolicyProfile,
};

pub(crate) fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub(crate) fn artifact(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, bytes, mode) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("package/{path}"), *bytes)
            .unwrap();
    }
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap()
}

// This shared integration-test module is compiled independently by several
// test targets; not every target exercises the adversarial archive fixture.
#[allow(dead_code)]
pub(crate) fn symlink_artifact() -> Vec<u8> {
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_link_name("/etc/passwd").unwrap();
    header.set_cksum();
    builder
        .append_data(&mut header, "package/bin/tool", std::io::empty())
        .unwrap();
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap()
}

pub(crate) fn flake_lock() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "nodes": {
            "nixpkgs": {
                "locked": {
                    "lastModified": 1782467914_u64,
                    "narHash": "sha256-pGvFkM8N0xEkIIXDe5YYfbEAvHrk4IxBrjB/x8OomhE=",
                    "owner": "NixOS",
                    "repo": "nixpkgs",
                    "rev": "e73de5be04e0eff4190a1432b946d469c794e7b4",
                    "type": "github"
                },
                "original": {
                    "owner": "NixOS",
                    "repo": "nixpkgs",
                    "rev": "e73de5be04e0eff4190a1432b946d469c794e7b4",
                    "type": "github"
                }
            },
            "root": { "inputs": { "nixpkgs": "nixpkgs" } }
        },
        "root": "root",
        "version": 7
    }))
    .unwrap()
}

pub(crate) fn plan(artifact: &[u8], bins: BTreeMap<String, String>) -> NixExportPlan {
    NixExportPlan {
        schema: NIX_EXPORT_PLAN_SCHEMA_V1,
        package: NixPackageIdentity {
            org: "example".into(),
            name: "sample".into(),
            version: "1.2.3".into(),
            target: None,
        },
        package_class: if bins.is_empty() {
            PlannedPackageClass::Data
        } else {
            PlannedPackageClass::PrebuiltBin
        },
        intent: ResolvedNixIntent {
            mode: NixExportMode::Artifact,
            attribute: "sample".into(),
            systems: vec!["aarch64-linux".into(), "x86_64-linux".into()],
            outputs: vec!["out".into()],
        },
        source: PlannedZedArtifact {
            file_name: "example-sample-1.2.3.tar.gz".into(),
            artifact: NixInteropArtifact {
                format: zed_interfaces::ArtifactFormat::TarGz,
                sha256: sha256(artifact),
                size: artifact.len() as u64,
            },
            manifest_sha256: "1".repeat(64),
            lock_sha256: "2".repeat(64),
        },
        bins,
        dependencies: Vec::<PlannedDependency>::new(),
        policy: NixPolicyEvidence {
            profile: NixPolicyProfile::StrictV1,
            pure_evaluation: true,
            import_from_derivation: false,
            sandbox_required: true,
            builder_network: NixBuilderNetwork::Disabled,
            dirty_source: false,
            publishable: true,
        },
    }
}
