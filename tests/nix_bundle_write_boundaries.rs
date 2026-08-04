use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

fn zed() -> &'static str {
    env!("CARGO_BIN_EXE_zed")
}

fn flake_lock() -> Vec<u8> {
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

fn write_project(root: &Path) {
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(
        root.join(".zpkg.toml"),
        r#"[package]
org = "acme"
name = "boundary-data"
version = "1.0.0"
description = "bundle writer boundary fixture"
license = "MIT"

[package.repository]
url = "https://github.com/acme/boundary-data"

[publish.nix]
attribute = "boundary-data"
systems = ["x86_64-linux"]
outputs = ["out"]
"#,
    )
    .unwrap();
    fs::write(root.join(".zpkg.lock"), "version = 1\n").unwrap();
    fs::write(root.join("data/value.txt"), b"immutable payload\n").unwrap();
}

fn run_bundle(
    cwd: &Path,
    lock: &Path,
    output: &Path,
    additional_env: &[(&str, &str)],
) -> Output {
    let mut command = Command::new(zed());
    command
        .current_dir(cwd)
        .env_clear()
        .args([
            "interop",
            "nix",
            "bundle",
            "write",
            "--frozen",
            "--flake-lock",
        ])
        .arg(lock)
        .arg("--out")
        .arg(output)
        .arg("--json");
    for (key, value) in additional_env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn parent_traversal_output_is_rejected_without_escape_or_project_mutation() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let lock = root.path().join("approved-flake.lock");
    write_project(&project);
    fs::write(&lock, flake_lock()).unwrap();
    let manifest_before = fs::read(project.join(".zpkg.toml")).unwrap();
    let zed_lock_before = fs::read(project.join(".zpkg.lock")).unwrap();

    let nested = project.join("src/deep");
    let lexical_output = Path::new("../escaped-bundle");
    let escaped = project.join("src/escaped-bundle");
    let output = run_bundle(&nested, &lock, lexical_output, &[]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("must not contain `..`"),
        "{}",
        stderr(&output)
    );
    assert!(!escaped.exists());
    assert_eq!(fs::read(project.join(".zpkg.toml")).unwrap(), manifest_before);
    assert_eq!(fs::read(project.join(".zpkg.lock")).unwrap(), zed_lock_before);
}

#[test]
fn missing_output_parent_is_not_created() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let lock = root.path().join("approved-flake.lock");
    let missing_parent = root.path().join("caller-must-create-this-parent");
    let output_path = missing_parent.join("bundle");
    write_project(&project);
    fs::write(&lock, flake_lock()).unwrap();

    let output = run_bundle(&project, &lock, &output_path, &[]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("inspecting output parent"),
        "{}",
        stderr(&output)
    );
    assert!(!missing_parent.exists());
    assert!(!output_path.exists());
}

#[test]
fn malformed_flake_lock_fails_before_output_publication() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let lock = root.path().join("malformed-flake.lock");
    let output_parent = root.path().join("exports");
    let output_path = output_parent.join("bundle");
    write_project(&project);
    fs::write(&lock, b"{}\n").unwrap();
    fs::create_dir(&output_parent).unwrap();

    let output = run_bundle(&project, &lock, &output_path, &[]);

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("flake.lock") || stderr(&output).contains("Nixpkgs"),
        "{}",
        stderr(&output)
    );
    assert!(!output_path.exists());
    assert_eq!(fs::read_dir(&output_parent).unwrap().count(), 0);
}

#[test]
fn ambient_credentials_and_home_are_not_read_or_serialized() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let lock = root.path().join("approved-flake.lock");
    let output_parent = root.path().join("exports");
    let output_path = output_parent.join("bundle");
    let unused_home = root.path().join("zed-home-must-not-exist");
    let secret = "bundle-writer-secret-must-not-appear";
    write_project(&project);
    fs::write(&lock, flake_lock()).unwrap();
    fs::create_dir(&output_parent).unwrap();

    let output = run_bundle(
        &project,
        &lock,
        &output_path,
        &[
            ("ZED_PKG_TOKEN", secret),
            ("ZED_PKG_SUPABASE_KEY", secret),
            ("ZED_PKG_AUTH_PASSWORD", secret),
            ("ZED_PKG_HOME", unused_home.to_str().unwrap()),
            (
                "ZED_PKG_REGISTRY",
                "https://person:secret@example.invalid/private-registry",
            ),
        ],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let receipt_text = stdout(&output);
    assert!(!receipt_text.contains(secret));
    assert!(!receipt_text.contains("person:secret"));
    assert!(!receipt_text.contains(&unused_home.to_string_lossy().to_string()));
    let receipt: Value = serde_json::from_str(receipt_text.trim()).unwrap();
    assert_eq!(receipt["schema"], "zed.nix-flake-bundle-write/v1");
    assert_eq!(receipt["outcome"], "created");
    assert!(!unused_home.exists());
    assert!(output_path.join("metadata/bundle.json").is_file());
}
