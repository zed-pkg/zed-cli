use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::{Compression, GzBuilder};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use walkdir::WalkDir;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::paths::LOCKFILE_FILE;
use zed_interfaces::registry::VersionMetadata;

fn zed() -> &'static str {
    env!("CARGO_BIN_EXE_zed")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn file_registry_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn create_artifact(registry: &Path, files: &[(&str, &[u8])]) -> (String, u64) {
    let archive_dir = TempDir::new().unwrap();
    let archive_path = archive_dir.path().join("artifact.tar.gz");
    let file = fs::File::create(&archive_path).unwrap();
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(file, Compression::default());
    let mut archive = tar::Builder::new(encoder);
    archive.mode(tar::HeaderMode::Deterministic);

    for (path, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(if path.ends_with(".sh") { 0o755 } else { 0o644 });
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_cksum();
        archive
            .append_data(&mut header, format!("pkg/{path}"), Cursor::new(*bytes))
            .unwrap();
    }

    let encoder = archive.into_inner().unwrap();
    encoder.finish().unwrap();
    let bytes = fs::read(archive_path).unwrap();
    let digest = sha256(&bytes);
    let size = bytes.len() as u64;
    let destination = registry.join("artifacts").join(format!("{digest}.tar.gz"));
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(destination, bytes).unwrap();
    (digest, size)
}

fn version_path(registry: &Path, org: &str, name: &str, version: &str) -> PathBuf {
    registry
        .join("packages")
        .join(org)
        .join(name)
        .join("versions")
        .join(format!("{version}.json"))
}

fn write_version(registry: &Path, metadata: &VersionMetadata) {
    let path = version_path(registry, &metadata.org, &metadata.name, &metadata.version);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_string_pretty(metadata).unwrap()).unwrap();
}

fn locked_package(
    registry: &Path,
    org: &str,
    name: &str,
    version: &str,
    digest: &str,
    size: u64,
) -> (LockedPackage, VersionMetadata) {
    let commit = Some("0123456789abcdef0123456789abcdef01234567".to_string());
    let metadata = VersionMetadata {
        org: org.to_string(),
        name: name.to_string(),
        version: version.to_string(),
        sha256: digest.to_string(),
        size,
        format: ArtifactFormat::TarGz,
        vcs_tag: format!("v{version}"),
        vcs_commit: commit.clone(),
        download_url: "ignored-by-file-registry".to_string(),
        published_at: "1970-01-01T00:00:00Z".to_string(),
        yanked: false,
    };
    let locked = LockedPackage {
        org: org.to_string(),
        name: name.to_string(),
        version: version.to_string(),
        sha256: digest.to_string(),
        size,
        format: ArtifactFormat::TarGz,
        vcs_tag: format!("v{version}"),
        vcs_commit: commit,
        source: file_registry_url(registry),
    };
    (locked, metadata)
}

fn write_lock(project: &Path, packages: Vec<LockedPackage>) -> Vec<u8> {
    let lock = Lockfile {
        version: Lockfile::CURRENT_VERSION,
        packages,
        native_dependencies: Vec::new(),
        nix_adapters: Vec::new(),
    };
    let bytes = lock.to_toml_string().unwrap().into_bytes();
    fs::write(project.join(LOCKFILE_FILE), &bytes).unwrap();
    bytes
}

fn command(project: &Path, home: &Path) -> Command {
    let mut command = Command::new(zed());
    command
        .current_dir(project)
        .env_clear()
        .arg("--home")
        .arg(home)
        .arg("fetch");
    command
}

fn fetch(project: &Path, home: &Path, output: &Path) -> Output {
    command(project, home)
        .arg("--frozen")
        .arg("--output")
        .arg(output)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[derive(Debug, PartialEq, Eq)]
struct SnapshotEntry {
    bytes: Vec<u8>,
    executable: bool,
}

fn snapshot(root: &Path) -> BTreeMap<String, SnapshotEntry> {
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            entry.metadata().unwrap().permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        files.insert(
            relative,
            SnapshotEntry {
                bytes: fs::read(entry.path()).unwrap(),
                executable,
            },
        );
    }
    files
}

fn assert_no_fetch_temporary_directories(parent: &Path) {
    for entry in fs::read_dir(parent).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !name.starts_with(".zed-fetch-"),
            "temporary fetch directory escaped cleanup: {name}"
        );
    }
}

#[test]
fn missing_lock_fails_without_output_or_source_mutation() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let outputs = root.path().join("outputs");
    let home = root.path().join("global-home");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&outputs).unwrap();
    fs::write(project.join("owned.txt"), b"consumer-owned\n").unwrap();
    let before = snapshot(&project);
    let destination = outputs.join("missing-lock");

    let result = fetch(&project, &home, &destination);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("requires .zpkg.lock"));
    assert!(!destination.exists());
    assert_eq!(snapshot(&project), before);
    assert!(!home.exists());
    assert_no_fetch_temporary_directories(&outputs);
}

#[test]
fn empty_lock_uses_environment_flags_and_replays_identically() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let outputs = root.path().join("outputs");
    let home = root.path().join("global-home");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&outputs).unwrap();
    let lock_bytes = write_lock(&project, Vec::new());

    let first = outputs.join("first");
    let first_result = command(&project, &home)
        .env("ZED_PKG_FROZEN", "yes")
        .env("ZED_PKG_FETCH_OUTPUT", &first)
        .output()
        .unwrap();
    assert!(first_result.status.success(), "{}", stderr(&first_result));

    let second = outputs.join("second");
    let second_result = fetch(&project, &home, &second);
    assert!(second_result.status.success(), "{}", stderr(&second_result));

    assert_eq!(snapshot(&first), snapshot(&second));
    assert_eq!(
        fs::read_to_string(first.join("metadata/lock.sha256")).unwrap(),
        format!("{}  .zpkg.lock\n", sha256(&lock_bytes))
    );
    let index: Value =
        serde_json::from_str(&fs::read_to_string(first.join("metadata/index.json")).unwrap())
            .unwrap();
    assert_eq!(index["schema"], "zed.fetch/v1");
    assert_eq!(index["packages"].as_array().unwrap().len(), 0);
    assert_eq!(fs::read_dir(first.join("packages")).unwrap().count(), 0);
    assert!(!home.exists());
    assert_no_fetch_temporary_directories(&outputs);
}

#[test]
fn multi_package_lock_is_sorted_and_deduplicates_one_artifact_payload() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let registry = root.path().join("registry");
    let outputs = root.path().join("outputs");
    let home = root.path().join("global-home");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&registry).unwrap();
    fs::create_dir_all(&outputs).unwrap();

    let (digest, size) = create_artifact(
        &registry,
        &[
            ("lib/value.txt", b"one immutable payload\n"),
            ("bin/tool.sh", b"#!/bin/sh\necho fetched\n"),
        ],
    );
    let (zeta, zeta_metadata) = locked_package(&registry, "acme", "zeta", "2.0.0", &digest, size);
    let (alpha, alpha_metadata) =
        locked_package(&registry, "acme", "alpha", "1.0.0", &digest, size);
    write_version(&registry, &zeta_metadata);
    write_version(&registry, &alpha_metadata);
    // Deliberately reverse lexical order in the lock. The exported index is
    // canonical even when the input writer preserved a different order.
    write_lock(&project, vec![zeta, alpha]);

    let destination = outputs.join("bundle");
    let result = fetch(&project, &home, &destination);
    assert!(result.status.success(), "{}", stderr(&result));

    let index_text = fs::read_to_string(destination.join("metadata/index.json")).unwrap();
    assert!(
        !index_text.contains(&registry.to_string_lossy().to_string()),
        "registry path leaked into portable metadata"
    );
    let index: Value = serde_json::from_str(&index_text).unwrap();
    let packages = index["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 2);
    assert_eq!(packages[0]["name"], "alpha");
    assert_eq!(packages[1]["name"], "zeta");
    assert_eq!(packages[0]["path"], packages[1]["path"]);
    assert_eq!(
        fs::read_dir(destination.join("packages"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .count(),
        1,
        "identical content-addressed artifacts must be copied once"
    );
    assert_eq!(
        fs::read(
            destination
                .join("packages")
                .join(&digest)
                .join("pkg/lib/value.txt")
        )
        .unwrap(),
        b"one immutable payload\n"
    );
    assert!(!home.exists());
    assert_no_fetch_temporary_directories(&outputs);
}

#[test]
fn registry_size_and_vcs_drift_fail_before_output_commit() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let registry = root.path().join("registry");
    let outputs = root.path().join("outputs");
    let home = root.path().join("global-home");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&registry).unwrap();
    fs::create_dir_all(&outputs).unwrap();

    let (digest, size) = create_artifact(&registry, &[("payload.txt", b"trusted\n")]);
    let (locked, mut metadata) =
        locked_package(&registry, "acme", "provenance", "1.0.0", &digest, size);
    write_lock(&project, vec![locked]);

    metadata.size += 1;
    write_version(&registry, &metadata);
    let size_output = outputs.join("size-drift");
    let result = fetch(&project, &home, &size_output);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("artifact size changed"));
    assert!(!size_output.exists());

    metadata.size = size;
    metadata.vcs_tag = "unexpected-tag".to_string();
    write_version(&registry, &metadata);
    let vcs_output = outputs.join("vcs-drift");
    let result = fetch(&project, &home, &vcs_output);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("VCS provenance changed"));
    assert!(!vcs_output.exists());

    assert!(!home.exists());
    assert_no_fetch_temporary_directories(&outputs);
}
