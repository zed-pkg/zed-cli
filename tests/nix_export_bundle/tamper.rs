use std::collections::BTreeMap;
use std::io::Write;

use serde_json::json;
use zed_cli::nix_export_bundle::render_nix_export_bundle;

use super::common::{artifact, flake_lock, plan, sha256, symlink_artifact};

#[test]
fn rejects_artifact_hash_and_size_drift() {
    let artifact_bytes = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let export_plan = plan(&artifact_bytes, BTreeMap::new());
    let mut tampered = artifact_bytes.clone();
    tampered.push(0);
    let error = render_nix_export_bundle(&export_plan, &tampered, &flake_lock())
        .unwrap_err()
        .to_string();
    assert!(error.contains("size drift") || error.contains("SHA-256 drift"));

    let mut wrong_size = export_plan.clone();
    wrong_size.source.artifact.size += 1;
    assert!(render_nix_export_bundle(&wrong_size, &artifact_bytes, &flake_lock()).is_err());
}

#[test]
fn rejects_mutable_unsupported_or_multi_input_flake_locks() {
    let artifact_bytes = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let export_plan = plan(&artifact_bytes, BTreeMap::new());
    let mut lock: serde_json::Value = serde_json::from_slice(&flake_lock()).unwrap();
    lock["nodes"]["nixpkgs"]["original"]
        .as_object_mut()
        .unwrap()
        .remove("rev");
    assert!(
        render_nix_export_bundle(
            &export_plan,
            &artifact_bytes,
            &serde_json::to_vec(&lock).unwrap(),
        )
        .is_err()
    );

    lock["nodes"]["nixpkgs"]["original"]["rev"] = json!("e73de5be04e0eff4190a1432b946d469c794e7b4");
    lock["nodes"]["nixpkgs"]["locked"]["narHash"] = json!("sha256-not-a-hash");
    assert!(
        render_nix_export_bundle(
            &export_plan,
            &artifact_bytes,
            &serde_json::to_vec(&lock).unwrap(),
        )
        .is_err()
    );

    let mut multi_input: serde_json::Value = serde_json::from_slice(&flake_lock()).unwrap();
    multi_input["nodes"]["extra"] = json!({
        "locked": {
            "narHash": "sha256-pGvFkM8N0xEkIIXDe5YYfbEAvHrk4IxBrjB/x8OomhE=",
            "owner": "example",
            "repo": "extra",
            "rev": "e73de5be04e0eff4190a1432b946d469c794e7b4",
            "type": "github"
        },
        "original": {
            "owner": "example",
            "repo": "extra",
            "rev": "e73de5be04e0eff4190a1432b946d469c794e7b4",
            "type": "github"
        }
    });
    multi_input["nodes"]["root"]["inputs"]["extra"] = json!("extra");
    assert!(
        render_nix_export_bundle(
            &export_plan,
            &artifact_bytes,
            &serde_json::to_vec(&multi_input).unwrap(),
        )
        .is_err()
    );
}

#[test]
fn rejects_unsorted_or_duplicate_system_declarations() {
    let artifact_bytes = artifact(&[("data/value.txt", b"payload\n", 0o644)]);

    let mut unsorted = plan(&artifact_bytes, BTreeMap::new());
    unsorted.intent.systems = vec!["x86_64-linux".into(), "aarch64-linux".into()];
    assert!(render_nix_export_bundle(&unsorted, &artifact_bytes, &flake_lock()).is_err());

    let mut duplicate = plan(&artifact_bytes, BTreeMap::new());
    duplicate.intent.systems = vec!["x86_64-linux".into(), "x86_64-linux".into()];
    assert!(render_nix_export_bundle(&duplicate, &artifact_bytes, &flake_lock()).is_err());
}

#[test]
fn rejects_non_executable_bins_unsafe_paths_archive_links_and_duplicates() {
    let non_executable_artifact = artifact(&[("bin/sample", b"payload", 0o644)]);
    let non_executable_bins = BTreeMap::from([("sample".into(), "bin/sample".into())]);
    let non_executable_plan = plan(&non_executable_artifact, non_executable_bins);
    assert!(
        render_nix_export_bundle(
            &non_executable_plan,
            &non_executable_artifact,
            &flake_lock(),
        )
        .is_err()
    );

    let unsafe_path_artifact = artifact(&[("bin/sample", b"payload", 0o755)]);
    let unsafe_path_bins = BTreeMap::from([("sample".into(), "../secret".into())]);
    let unsafe_path_plan = plan(&unsafe_path_artifact, unsafe_path_bins);
    assert!(
        render_nix_export_bundle(&unsafe_path_plan, &unsafe_path_artifact, &flake_lock()).is_err()
    );

    let linked_artifact = symlink_artifact();
    let linked_bins = BTreeMap::from([("sample".into(), "bin/tool".into())]);
    let linked_plan = plan(&linked_artifact, linked_bins);
    assert!(render_nix_export_bundle(&linked_plan, &linked_artifact, &flake_lock()).is_err());

    let duplicate_artifact = artifact(&[
        ("data/value.txt", &b"first"[..], 0o644),
        ("data/value.txt", &b"second"[..], 0o644),
    ]);
    let duplicate_plan = plan(&duplicate_artifact, BTreeMap::new());
    assert!(render_nix_export_bundle(&duplicate_plan, &duplicate_artifact, &flake_lock()).is_err());
}

#[test]
fn canonical_inventory_detects_post_render_mutation() {
    let artifact_bytes = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let export_plan = plan(&artifact_bytes, BTreeMap::new());
    let mut rendered =
        render_nix_export_bundle(&export_plan, &artifact_bytes, &flake_lock()).unwrap();
    rendered
        .files
        .get_mut("README.md")
        .unwrap()
        .write_all(b"tampered")
        .unwrap();
    assert!(rendered.validate().is_err());

    let mut rendered =
        render_nix_export_bundle(&export_plan, &artifact_bytes, &flake_lock()).unwrap();
    rendered
        .files
        .get_mut("flake.lock")
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    assert!(rendered.validate().is_err());

    let mut wrong_digest = export_plan.clone();
    wrong_digest.source.artifact.sha256 = sha256(b"different");
    assert!(render_nix_export_bundle(&wrong_digest, &artifact_bytes, &flake_lock()).is_err());
}
