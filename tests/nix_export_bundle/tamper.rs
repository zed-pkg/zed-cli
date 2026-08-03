use std::collections::BTreeMap;
use std::io::Write;

use serde_json::json;
use zed_cli::nix_export_bundle::render_nix_export_bundle;

use super::common::{artifact, flake_lock, plan, sha256, symlink_artifact};

#[test]
fn rejects_artifact_hash_and_size_drift() {
    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let plan = plan(&artifact, BTreeMap::new());
    let mut tampered = artifact.clone();
    tampered.push(0);
    let error = render_nix_export_bundle(&plan, &tampered, &flake_lock())
        .unwrap_err()
        .to_string();
    assert!(error.contains("size drift") || error.contains("SHA-256 drift"));

    let mut wrong_size = plan.clone();
    wrong_size.source.artifact.size += 1;
    assert!(render_nix_export_bundle(&wrong_size, &artifact, &flake_lock()).is_err());
}

#[test]
fn rejects_mutable_unsupported_or_multi_input_flake_locks() {
    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let plan = plan(&artifact, BTreeMap::new());
    let mut lock: serde_json::Value = serde_json::from_slice(&flake_lock()).unwrap();
    lock["nodes"]["nixpkgs"]["original"]
        .as_object_mut()
        .unwrap()
        .remove("rev");
    assert!(
        render_nix_export_bundle(&plan, &artifact, &serde_json::to_vec(&lock).unwrap())
            .is_err()
    );

    lock["nodes"]["nixpkgs"]["original"]["rev"] =
        json!("e73de5be04e0eff4190a1432b946d469c794e7b4");
    lock["nodes"]["nixpkgs"]["locked"]["narHash"] = json!("sha256-not-a-hash");
    assert!(
        render_nix_export_bundle(&plan, &artifact, &serde_json::to_vec(&lock).unwrap())
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
            &plan,
            &artifact,
            &serde_json::to_vec(&multi_input).unwrap(),
        )
        .is_err()
    );
}

#[test]
fn rejects_unsorted_or_duplicate_system_declarations() {
    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);

    let mut unsorted = plan(&artifact, BTreeMap::new());
    unsorted.intent.systems = vec!["x86_64-linux".into(), "aarch64-linux".into()];
    assert!(render_nix_export_bundle(&unsorted, &artifact, &flake_lock()).is_err());

    let mut duplicate = plan(&artifact, BTreeMap::new());
    duplicate.intent.systems = vec!["x86_64-linux".into(), "x86_64-linux".into()];
    assert!(render_nix_export_bundle(&duplicate, &artifact, &flake_lock()).is_err());
}

#[test]
fn rejects_non_executable_bins_unsafe_paths_archive_links_and_duplicates() {
    let non_executable = artifact(&[("bin/sample", b"payload", 0o644)]);
    let bins = BTreeMap::from([("sample".into(), "bin/sample".into())]);
    let plan = plan(&non_executable, bins);
    assert!(render_nix_export_bundle(&plan, &non_executable, &flake_lock()).is_err());

    let artifact = artifact(&[("bin/sample", b"payload", 0o755)]);
    let bins = BTreeMap::from([("sample".into(), "../secret".into())]);
    let plan = plan(&artifact, bins);
    assert!(render_nix_export_bundle(&plan, &artifact, &flake_lock()).is_err());

    let linked = symlink_artifact();
    let bins = BTreeMap::from([("sample".into(), "bin/tool".into())]);
    let plan = plan(&linked, bins);
    assert!(render_nix_export_bundle(&plan, &linked, &flake_lock()).is_err());

    let duplicated = artifact(&[
        ("data/value.txt", b"first", 0o644),
        ("data/value.txt", b"second", 0o644),
    ]);
    let plan = plan(&duplicated, BTreeMap::new());
    assert!(render_nix_export_bundle(&plan, &duplicated, &flake_lock()).is_err());
}

#[test]
fn canonical_inventory_detects_post_render_mutation() {
    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let plan = plan(&artifact, BTreeMap::new());
    let mut rendered = render_nix_export_bundle(&plan, &artifact, &flake_lock()).unwrap();
    rendered
        .files
        .get_mut("README.md")
        .unwrap()
        .write_all(b"tampered")
        .unwrap();
    assert!(rendered.validate().is_err());

    let mut rendered = render_nix_export_bundle(&plan, &artifact, &flake_lock()).unwrap();
    rendered
        .files
        .get_mut("flake.lock")
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    assert!(rendered.validate().is_err());

    let mut wrong_digest = plan.clone();
    wrong_digest.source.artifact.sha256 = sha256(b"different");
    assert!(render_nix_export_bundle(&wrong_digest, &artifact, &flake_lock()).is_err());
}
