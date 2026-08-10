use std::fs;
use std::path::{Path, PathBuf};

use zed_cli::config::Config;
use zed_cli::install_graph::prefetch;
use zed_cli::pack::pack;
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::registry::VersionMetadata;

fn test_config(registry: &Path, home: PathBuf) -> Config {
    Config {
        registry: format!("file://{}", registry.display()),
        home,
        token: None,
        auth_url: "http://127.0.0.1/unused".to_string(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
    }
}

#[test]
fn frozen_prefetch_restores_from_the_store_after_registry_metadata_disappears() {
    let temp = tempfile::tempdir().unwrap();
    let registry = temp.path().join("registry");
    let source = temp.path().join("source");
    let packed_dir = temp.path().join("packed");
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&project).unwrap();

    let manifest_text = r#"[package]
org = "offline"
name = "tool"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/offline/tool"
"#;
    fs::write(source.join(MANIFEST_FILE), manifest_text).unwrap();
    fs::write(source.join("payload.txt"), "offline/tool@1.0.0\n").unwrap();
    let manifest = Manifest::parse(manifest_text).unwrap();
    let packed = pack(&source, &manifest, Some(&packed_dir)).unwrap();

    let artifact_dir = registry.join("artifacts");
    fs::create_dir_all(&artifact_dir).unwrap();
    let artifact = artifact_dir.join(format!("{}.tar.gz", packed.sha256));
    fs::copy(&packed.path, &artifact).unwrap();

    let version_dir = registry
        .join("packages")
        .join("offline")
        .join("tool")
        .join("versions");
    fs::create_dir_all(&version_dir).unwrap();
    let version = VersionMetadata {
        org: "offline".to_string(),
        name: "tool".to_string(),
        version: "1.0.0".to_string(),
        sha256: packed.sha256.clone(),
        size: packed.size,
        format: packed.format,
        vcs_tag: "v1.0.0".to_string(),
        vcs_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        download_url: format!("file://{}", artifact.display()),
        published_at: "1970-01-01T00:00:00Z".to_string(),
        yanked: false,
    };
    fs::write(
        version_dir.join("1.0.0.json"),
        serde_json::to_string_pretty(&version).unwrap(),
    )
    .unwrap();

    let mut lock = Lockfile::default();
    lock.upsert(LockedPackage {
        org: version.org,
        name: version.name,
        version: version.version,
        sha256: version.sha256,
        size: version.size,
        format: version.format,
        vcs_tag: version.vcs_tag,
        vcs_commit: version.vcs_commit,
        source: format!("file://{}", registry.display()),
    });
    fs::write(project.join(LOCKFILE_FILE), lock.to_toml_string().unwrap()).unwrap();

    let cfg = test_config(&registry, home);
    let cold = prefetch(&project, &cfg, true).unwrap();
    assert_eq!(cold.resolved, 1);
    assert_eq!(cold.downloaded, 1);

    fs::remove_dir_all(&registry).unwrap();
    let offline = prefetch(&project, &cfg, true).unwrap();
    assert_eq!(offline.resolved, 1);
    assert_eq!(offline.downloaded, 0);
}
