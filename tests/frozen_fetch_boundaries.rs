use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::lockfile::{LockedPackage, Lockfile};
use zed_interfaces::paths::LOCKFILE_FILE;

fn zed() -> &'static str {
    env!("CARGO_BIN_EXE_zed")
}

fn write_lock(project: &Path, source: Option<String>) {
    let packages = source
        .map(|source| {
            vec![LockedPackage {
                org: "acme".to_string(),
                name: "boundary".to_string(),
                version: "1.0.0".to_string(),
                sha256: "a".repeat(64),
                size: 1,
                format: ArtifactFormat::TarGz,
                vcs_tag: "v1.0.0".to_string(),
                vcs_commit: None,
                source,
            }]
        })
        .unwrap_or_default();
    let lock = Lockfile {
        version: Lockfile::CURRENT_VERSION,
        packages,
        native_dependencies: Vec::new(),
        tools: Vec::new(),
        nix_adapters: Vec::new(),
    };
    fs::write(project.join(LOCKFILE_FILE), lock.to_toml_string().unwrap()).unwrap();
}

fn fetch(project: &Path, home: &Path, output: &Path) -> Output {
    Command::new(zed())
        .current_dir(project)
        .env_clear()
        .arg("--home")
        .arg(home)
        .arg("fetch")
        .arg("--frozen")
        .arg("--output")
        .arg(output)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn empty_project(root: &TempDir) -> (PathBuf, PathBuf) {
    let project = root.path().join("project");
    let home = root.path().join("fetch-home-must-remain-absent");
    fs::create_dir(&project).unwrap();
    write_lock(&project, None);
    (project, home)
}

#[test]
fn nonexistent_output_parent_is_rejected_without_creation() {
    let root = TempDir::new().unwrap();
    let (project, home) = empty_project(&root);
    let missing_parent = root.path().join("missing-parent");
    let output = missing_parent.join("bundle");

    let result = fetch(&project, &home, &output);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("parent must already exist"));
    assert!(!missing_parent.exists());
    assert!(!output.exists());
    assert!(!home.exists());
}

#[test]
fn non_directory_output_parent_is_rejected_without_replacement() {
    let root = TempDir::new().unwrap();
    let (project, home) = empty_project(&root);
    let parent = root.path().join("caller-owned-file");
    fs::write(&parent, b"owned\n").unwrap();
    let output = parent.join("bundle");

    let result = fetch(&project, &home, &output);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("parent must already exist"));
    assert_eq!(fs::read(&parent).unwrap(), b"owned\n");
    assert!(!home.exists());
}

#[cfg(unix)]
#[test]
fn symlinked_parent_cannot_redirect_output_into_the_project() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let (project, home) = empty_project(&root);
    fs::write(project.join("owned.txt"), b"consumer-owned\n").unwrap();
    let redirect = root.path().join("redirect");
    symlink(&project, &redirect).unwrap();
    let output = redirect.join("generated");

    let result = fetch(&project, &home, &output);
    assert!(!result.status.success());
    assert!(stderr(&result).contains("canonical fetch output"));
    assert!(!project.join("generated").exists());
    assert_eq!(
        fs::read(project.join("owned.txt")).unwrap(),
        b"consumer-owned\n"
    );
    assert!(!home.exists());
}

#[test]
fn file_registry_query_secret_is_rejected_without_echo_or_output() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let outputs = root.path().join("outputs");
    let home = root.path().join("fetch-home-must-remain-absent");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&outputs).unwrap();
    let secret = "super-secret-query-value";
    write_lock(
        &project,
        Some(format!("file://{}?token={secret}", outputs.display())),
    );
    let output = outputs.join("bundle");

    let result = fetch(&project, &home, &output);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("file registry sources may not embed"));
    assert!(!message.contains(secret));
    assert!(!output.exists());
    assert!(!home.exists());
}

#[test]
fn non_local_file_registry_authority_fails_closed_without_source_echo() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let outputs = root.path().join("outputs");
    let home = root.path().join("fetch-home-must-remain-absent");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&outputs).unwrap();
    let source = "file://remote-registry.invalid/private/path";
    write_lock(&project, Some(source.to_string()));
    let output = outputs.join("bundle");

    let result = fetch(&project, &home, &output);
    assert!(!result.status.success());
    let message = stderr(&result);
    assert!(message.contains("not a local absolute path"));
    assert!(!message.contains(source));
    assert!(!output.exists());
    assert!(!home.exists());
}
