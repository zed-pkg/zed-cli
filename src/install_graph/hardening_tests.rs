use anyhow::anyhow;

use super::*;
use crate::pack::pack;
use crate::registry::registry_for;

fn manifest_text(org: &str, name: &str, version: &str) -> String {
    format!(
        r#"[package]
org = "{org}"
name = "{name}"
version = "{version}"

[package.repository]
vcs = "git"
url = "https://example.invalid/{org}/{name}"
"#,
    )
}

fn prepared_artifact(
    temp: &tempfile::TempDir,
    stored_manifest: Option<&str>,
) -> (Store, FetchTask, Box<dyn Registry>) {
    let source = temp.path().join("source");
    fs::create_dir_all(&source).unwrap();
    let selected_manifest = manifest_text("test", "selected", "1.0.0");
    fs::write(source.join(MANIFEST_FILE), &selected_manifest).unwrap();
    fs::write(source.join("payload.txt"), b"fixture\n").unwrap();
    let manifest = Manifest::parse(&selected_manifest).unwrap();
    let packed = pack(&source, &manifest, Some(&temp.path().join("packed"))).unwrap();

    let store = Store::new(&temp.path().join("home"));
    let package_dir = store.add_artifact(&packed.path, &packed.sha256).unwrap();
    match stored_manifest {
        Some(text) => fs::write(package_dir.join(MANIFEST_FILE), text).unwrap(),
        None => fs::remove_file(package_dir.join(MANIFEST_FILE)).unwrap(),
    }

    let task = FetchTask {
        sequence: 0,
        key: "test/selected".to_string(),
        version: VersionMetadata {
            org: "test".to_string(),
            name: "selected".to_string(),
            version: "1.0.0".to_string(),
            sha256: packed.sha256,
            size: packed.size,
            format: packed.format,
            vcs_tag: "v1.0.0".to_string(),
            vcs_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            download_url: "file:///unused-artifact.tar.gz".to_string(),
            published_at: "1970-01-01T00:00:00Z".to_string(),
            yanked: false,
            mirrors: Vec::new(),
            signatures: Vec::new(),
        },
    };

    let registry_root = temp.path().join("registry");
    fs::create_dir_all(&registry_root).unwrap();
    let registry = registry_for(&format!("file://{}", registry_root.display())).unwrap();
    (store, task, registry)
}

#[test]
fn dependency_keys_are_slug_validated_before_registry_access() {
    for key in [
        "../escape",
        "test/../escape",
        "test/name/extra",
        "/name",
        "org/",
    ] {
        let error = super::artifact::split_key(key).unwrap_err().to_string();
        assert!(error.contains("invalid package spec"), "{key}: {error}");
    }

    assert_eq!(
        super::artifact::split_key("valid-org/valid-name").unwrap(),
        ("valid-org".to_string(), "valid-name".to_string())
    );
}

#[test]
fn malformed_embedded_manifests_fail_closed_instead_of_truncating_the_graph() {
    let temp = tempfile::tempdir().unwrap();
    let (store, task, registry) = prepared_artifact(&temp, Some("[package\ninvalid = true\n"));

    let error = super::artifact::prefetch_one(registry.as_ref(), &store, task).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("reading dependency manifest"), "{message}");
    assert!(message.contains("test/selected@1.0.0"), "{message}");
}

#[test]
fn embedded_manifest_identity_must_match_selected_registry_metadata() {
    let temp = tempfile::tempdir().unwrap();
    let mismatched = manifest_text("test", "different", "2.0.0");
    let (store, task, registry) = prepared_artifact(&temp, Some(&mismatched));

    let error = super::artifact::prefetch_one(registry.as_ref(), &store, task)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("artifact manifest declares test/different@2.0.0"),
        "{error}"
    );
    assert!(error.contains("test/selected@1.0.0"), "{error}");
}

#[test]
fn legacy_manifestless_artifacts_remain_valid_leaf_packages() {
    let temp = tempfile::tempdir().unwrap();
    let (store, task, registry) = prepared_artifact(&temp, None);

    let result = super::artifact::prefetch_one(registry.as_ref(), &store, task).unwrap();
    assert!(result.dependencies.is_empty());
    assert!(!result.downloaded);
}

#[test]
fn sequenced_receive_buffers_later_failures_until_earlier_results_arrive() {
    let queue = Arc::new(TaskQueue::default());
    let (sender, results) = mpsc::channel();
    let pool = FetchPool {
        queue,
        results,
        workers: Vec::new(),
    };

    sender
        .send(FetchMessage {
            sequence: 1,
            result: Err(anyhow!(
                "later lockfile entry failed first in wall-clock time"
            )),
        })
        .unwrap();
    sender
        .send(FetchMessage {
            sequence: 0,
            result: Ok(FetchOutcome {
                sequence: 0,
                dependencies: Vec::new(),
                downloaded: false,
            }),
        })
        .unwrap();
    drop(sender);

    let first = pool.recv_for_sequence(0).unwrap();
    assert_eq!(first.sequence, 0);
    assert_eq!(pool.pending.len(), 1);
    let error = pool.recv_for_sequence(1).unwrap_err().to_string();
    assert!(error.contains("later lockfile entry failed first"), "{error}");
}

#[test]
fn metadata_failures_are_reported_in_lockfile_order() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    fs::create_dir_all(&registry).unwrap();
    let manifest = temp.path().join(MANIFEST_FILE);
    fs::write(
        &manifest,
        manifest_text("test", "root", "1.0.0")
            + "\n[dependencies]\n\"test/b\" = \"1\"\n\"test/a\" = \"1\"\n",
    )
    .unwrap();
    let registry_url = format!("file://{}", registry.display());
    let options = InstallGraphOptions {
        manifest_path: Some(manifest),
        registry: Some(registry_url),
        ..InstallGraphOptions::default()
    };

    let error = install_graph(options).unwrap_err().to_string();
    assert!(error.contains("test/a"), "{error}");
}
