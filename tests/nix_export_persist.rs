use std::fs;
use std::sync::Arc;
use std::thread;

#[path = "nix_export_bundle/common.rs"]
mod common;

use tempfile::tempdir;
use zed_cli::nix_export_bundle::{
    PersistNixExportBundleOutcome, persist_nix_export_bundle, render_nix_export_bundle,
    verify_persisted_bundle,
};

fn rendered() -> zed_cli::nix_export_bundle::RenderedNixExportBundle {
    let artifact = common::artifact(&[("data/value.txt", b"payload\n", 0o644)]);
    let plan = common::plan(&artifact, Default::default());
    render_nix_export_bundle(&plan, &artifact, &common::flake_lock()).unwrap()
}

fn staging_entries(parent: &std::path::Path) -> Vec<String> {
    fs::read_dir(parent)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".zed-nix-bundle-") && name.ends_with(".tmp"))
        .collect()
}

#[test]
fn persists_fresh_bundle_and_accepts_exact_existing_state_without_rewrite() {
    let root = tempdir().unwrap();
    let destination = root.path().join("bundle");
    let rendered = rendered();

    assert_eq!(
        persist_nix_export_bundle(&rendered, &destination).unwrap(),
        PersistNixExportBundleOutcome::Created
    );
    verify_persisted_bundle(&rendered, &destination).unwrap();
    let before = fs::metadata(destination.join("metadata/bundle.json"))
        .unwrap()
        .modified()
        .unwrap();

    assert_eq!(
        persist_nix_export_bundle(&rendered, &destination).unwrap(),
        PersistNixExportBundleOutcome::AlreadyCurrent
    );
    let after = fs::metadata(destination.join("metadata/bundle.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after);
    assert!(staging_entries(root.path()).is_empty());
}

#[test]
fn rejects_changed_missing_extra_and_non_directory_destinations() {
    let rendered = rendered();

    let changed_root = tempdir().unwrap();
    let changed = changed_root.path().join("bundle");
    persist_nix_export_bundle(&rendered, &changed).unwrap();
    fs::write(changed.join("README.md"), b"changed\n").unwrap();
    assert!(persist_nix_export_bundle(&rendered, &changed).is_err());

    let missing_root = tempdir().unwrap();
    let missing = missing_root.path().join("bundle");
    persist_nix_export_bundle(&rendered, &missing).unwrap();
    fs::remove_file(missing.join("flake.lock")).unwrap();
    assert!(persist_nix_export_bundle(&rendered, &missing).is_err());

    let extra_root = tempdir().unwrap();
    let extra = extra_root.path().join("bundle");
    persist_nix_export_bundle(&rendered, &extra).unwrap();
    fs::write(extra.join("unexpected.txt"), b"unexpected\n").unwrap();
    assert!(persist_nix_export_bundle(&rendered, &extra).is_err());

    let file_root = tempdir().unwrap();
    let file = file_root.path().join("bundle");
    fs::write(&file, b"caller-owned\n").unwrap();
    assert!(persist_nix_export_bundle(&rendered, &file).is_err());
    assert_eq!(fs::read(&file).unwrap(), b"caller-owned\n");
}

#[cfg(unix)]
#[test]
fn rejects_changed_modes_and_symlinked_leaf_paths() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let rendered = rendered();
    let mode_root = tempdir().unwrap();
    let mode_destination = mode_root.path().join("bundle");
    persist_nix_export_bundle(&rendered, &mode_destination).unwrap();
    fs::set_permissions(
        mode_destination.join("package.nix"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(persist_nix_export_bundle(&rendered, &mode_destination).is_err());

    let destination_link_root = tempdir().unwrap();
    let real = destination_link_root.path().join("real");
    fs::create_dir(&real).unwrap();
    let destination_link = destination_link_root.path().join("bundle");
    symlink(&real, &destination_link).unwrap();
    assert!(persist_nix_export_bundle(&rendered, &destination_link).is_err());

    let parent_link_root = tempdir().unwrap();
    let real_parent = parent_link_root.path().join("real-parent");
    fs::create_dir(&real_parent).unwrap();
    let linked_parent = parent_link_root.path().join("linked-parent");
    symlink(&real_parent, &linked_parent).unwrap();
    assert!(persist_nix_export_bundle(&rendered, &linked_parent.join("bundle")).is_err());
}

#[cfg(unix)]
#[test]
fn canonicalizes_existing_parent_below_a_symlinked_ancestor() {
    use std::os::unix::fs::symlink;

    let rendered = rendered();
    let root = tempdir().unwrap();
    let real_ancestor = root.path().join("real-ancestor");
    let real_parent = real_ancestor.join("nested-parent");
    fs::create_dir_all(&real_parent).unwrap();
    let alias = root.path().join("ancestor-alias");
    symlink(&real_ancestor, &alias).unwrap();
    let destination = alias.join("nested-parent").join("bundle");

    assert_eq!(
        persist_nix_export_bundle(&rendered, &destination).unwrap(),
        PersistNixExportBundleOutcome::Created
    );
    verify_persisted_bundle(&rendered, &destination).unwrap();
    assert!(real_parent.join("bundle").is_dir());
    assert!(staging_entries(&real_parent).is_empty());
}

#[test]
fn concurrent_identical_writers_publish_once_and_leave_no_staging_state() {
    let root = tempdir().unwrap();
    let destination = root.path().join("bundle");
    let rendered = Arc::new(rendered());

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let rendered = Arc::clone(&rendered);
            let destination = destination.clone();
            thread::spawn(move || persist_nix_export_bundle(&rendered, &destination))
        })
        .collect();

    let mut created = 0;
    let mut current = 0;
    for handle in handles {
        match handle.join().unwrap().unwrap() {
            PersistNixExportBundleOutcome::Created => created += 1,
            PersistNixExportBundleOutcome::AlreadyCurrent => current += 1,
        }
    }
    assert_eq!(created, 1);
    assert_eq!(current, 7);
    verify_persisted_bundle(&rendered, &destination).unwrap();
    assert!(staging_entries(root.path()).is_empty());
}
