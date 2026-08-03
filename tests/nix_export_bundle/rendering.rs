use std::collections::BTreeMap;

use zed_cli::nix_export_bundle::{NIX_FLAKE_BUNDLE_SCHEMA_V1, render_nix_export_bundle};

use super::common::{artifact, flake_lock, plan};

#[test]
fn renders_byte_identical_sorted_bundles() {
    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let plan = plan(&artifact, BTreeMap::new());
    let first = render_nix_export_bundle(&plan, &artifact, &flake_lock()).unwrap();
    let second = render_nix_export_bundle(&plan, &artifact, &flake_lock()).unwrap();

    assert_eq!(first, second);
    assert_eq!(first.inventory.schema, NIX_FLAKE_BUNDLE_SCHEMA_V1);
    assert!(
        first
            .inventory
            .entries
            .windows(2)
            .all(|pair| pair[0].path < pair[1].path)
    );
    assert!(
        !first
            .inventory
            .entries
            .iter()
            .any(|entry| entry.path == "metadata/bundle.json")
    );
    first.validate().unwrap();
}

#[test]
fn preserves_approved_flake_lock_bytes_exactly() {
    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let plan = plan(&artifact, BTreeMap::new());
    let mut lock = flake_lock();
    lock.push(b'\n');

    let rendered = render_nix_export_bundle(&plan, &artifact, &lock).unwrap();
    assert_eq!(rendered.files["flake.lock"], lock);
    rendered.validate().unwrap();
}

#[test]
fn renders_prebuilt_bin_symlinks_only_for_executable_payloads() {
    let artifact = artifact(&[("bin/sample", b"#!/bin/sh\nexit 0\n", 0o755)]);
    let bins = BTreeMap::from([("sample".into(), "bin/sample".into())]);
    let plan = plan(&artifact, bins);
    let rendered = render_nix_export_bundle(&plan, &artifact, &flake_lock()).unwrap();
    let package = String::from_utf8(rendered.files["package.nix"].clone()).unwrap();
    assert!(package.contains("mkdir -p \"$out/bin\""));
    assert!(
        package.contains("ln -s \"$payloadRoot/bin/sample\" \"$out/bin/sample\"")
    );
}

#[test]
fn data_package_has_no_executable_install_surface() {
    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let plan = plan(&artifact, BTreeMap::new());
    let rendered = render_nix_export_bundle(&plan, &artifact, &flake_lock()).unwrap();
    let package = String::from_utf8(rendered.files["package.nix"].clone()).unwrap();
    assert!(!package.contains("$out/bin"));
    assert!(package.contains("share/zed-pkg/example/sample/1.2.3"));
}

#[test]
fn output_contains_no_ambient_secret_or_absolute_workspace_path() {
    let artifact = artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let plan = plan(&artifact, BTreeMap::new());
    let rendered = render_nix_export_bundle(&plan, &artifact, &flake_lock()).unwrap();
    let all = rendered
        .files
        .values()
        .flat_map(|bytes| bytes.iter().copied())
        .collect::<Vec<_>>();
    let text = String::from_utf8_lossy(&all);
    assert!(!text.contains("must-not-appear"));
    assert!(!text.contains("/home/alex/workspace"));
    assert!(!text.contains("registry.example.test"));
}
