use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use walkdir::WalkDir;

fn zed() -> &'static str {
    env!("CARGO_BIN_EXE_zed")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
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

fn manifest(system: &str) -> String {
    format!(
        r#"[package]
org = "acme"
name = "dataset"
version = "1.2.3"
description = "immutable data"
license = "MIT"

[package.repository]
url = "https://github.com/acme/dataset"

[publish.nix]
attribute = "dataset"
systems = ["{system}"]
outputs = ["out"]
"#
    )
}

fn write_project(root: &Path, system: &str) {
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(root.join(".zpkg.toml"), manifest(system)).unwrap();
    fs::write(root.join(".zpkg.lock"), "version = 1\n").unwrap();
    fs::write(root.join("data/value.txt"), b"same immutable payload\n").unwrap();
}

fn run_bundle(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(zed());
    command
        .current_dir(cwd)
        .env_clear()
        .arg("interop")
        .arg("nix")
        .arg("bundle")
        .arg("write");
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            result.insert(relative, sha256(&fs::read(entry.path()).unwrap()));
        }
    }
    result
}

fn assert_bundle_shape(bundle: &Path) {
    for relative in [
        "flake.nix",
        "flake.lock",
        "package.nix",
        "README.md",
        "metadata/plan.json",
        "metadata/bundle.json",
        "artifacts/acme-dataset-1.2.3.tar.gz",
    ] {
        assert!(bundle.join(relative).is_file(), "missing {relative}");
    }
}

#[test]
fn command_creates_idempotent_no_clobber_bundle_from_nested_directory() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let lock = root.path().join("approved-flake.lock");
    let output_parent = root.path().join("exports");
    let bundle = output_parent.join("dataset-flake");
    write_project(&project, "x86_64-linux");
    fs::write(&lock, flake_lock()).unwrap();
    fs::create_dir(&output_parent).unwrap();
    let canonical_bundle = fs::canonicalize(&output_parent)
        .unwrap()
        .join("dataset-flake");
    let project_before = snapshot(&project);

    let first = run_bundle(
        &project.join("src/deep"),
        &[
            "--frozen",
            "--flake-lock",
            lock.to_str().unwrap(),
            "--out",
            bundle.to_str().unwrap(),
            "--json",
        ],
        &[],
    );
    assert!(first.status.success(), "{}", stderr(&first));
    let first_receipt: Value = serde_json::from_str(stdout(&first).trim()).unwrap();
    assert_eq!(first_receipt["schema"], "zed.nix-flake-bundle-write/v1");
    assert_eq!(first_receipt["outcome"], "created");
    assert_eq!(first_receipt["package"]["org"], "acme");
    assert_eq!(first_receipt["package"]["name"], "dataset");
    assert_eq!(
        first_receipt["destination"],
        canonical_bundle.to_string_lossy().as_ref()
    );
    assert_eq!(
        first_receipt["bundle_sha256"].as_str().unwrap().len(),
        64
    );
    assert_bundle_shape(&bundle);
    assert_eq!(snapshot(&project), project_before);
    let bundle_before = snapshot(&bundle);

    let second = run_bundle(
        &project,
        &[
            "--frozen",
            "--flake-lock",
            lock.to_str().unwrap(),
            "--output",
            bundle.to_str().unwrap(),
            "--json",
        ],
        &[],
    );
    assert!(second.status.success(), "{}", stderr(&second));
    let second_receipt: Value = serde_json::from_str(stdout(&second).trim()).unwrap();
    assert_eq!(second_receipt["outcome"], "already-current");
    assert_eq!(first_receipt["bundle_sha256"], second_receipt["bundle_sha256"]);
    assert_eq!(snapshot(&bundle), bundle_before);

    fs::write(bundle.join("README.md"), b"tampered\n").unwrap();
    let tampered = run_bundle(
        &project,
        &[
            "--frozen",
            "--flake-lock",
            lock.to_str().unwrap(),
            "--out",
            bundle.to_str().unwrap(),
        ],
        &[],
    );
    assert!(!tampered.status.success());
    assert!(
        stderr(&tampered).contains("differs from rendered bytes"),
        "{}",
        stderr(&tampered)
    );
    assert_eq!(fs::read(bundle.join("README.md")).unwrap(), b"tampered\n");
}

#[test]
fn command_accepts_environment_contract_and_rejects_unknown_flags() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let lock = root.path().join("approved-flake.lock");
    let output_parent = root.path().join("exports");
    let bundle = output_parent.join("env-flake");
    write_project(&project, "x86_64-linux");
    fs::write(&lock, flake_lock()).unwrap();
    fs::create_dir(&output_parent).unwrap();

    let environment = run_bundle(
        &project,
        &[],
        &[
            ("ZED_PKG_FROZEN", "yes"),
            ("ZED_PKG_NIX_PLAN_JSON", "on"),
            ("ZED_PKG_NIX_FLAKE_LOCK", lock.to_str().unwrap()),
            ("ZED_PKG_NIX_BUNDLE_OUT", bundle.to_str().unwrap()),
        ],
    );
    assert!(environment.status.success(), "{}", stderr(&environment));
    let receipt: Value = serde_json::from_str(stdout(&environment).trim()).unwrap();
    assert_eq!(receipt["outcome"], "created");

    let unknown = run_bundle(
        &project,
        &[
            "--frozen",
            "--flake-lock",
            lock.to_str().unwrap(),
            "--out",
            root.path().join("unknown").to_str().unwrap(),
            "--definitely-unknown",
        ],
        &[],
    );
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("unknown Nix interop option"));
}

#[test]
#[ignore = "requires Nix and one explicit immutable closure acquisition step"]
fn persisted_command_bundle_checks_and_builds_offline() {
    let current_system = Command::new("nix")
        .args([
            "eval",
            "--impure",
            "--raw",
            "--expr",
            "builtins.currentSystem",
        ])
        .output()
        .expect("querying builtins.currentSystem");
    assert!(current_system.status.success());
    let current_system = String::from_utf8(current_system.stdout).unwrap();

    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let lock = root.path().join("approved-flake.lock");
    let bundle = root.path().join("bundle");
    write_project(&project, current_system.trim());
    fs::write(&lock, flake_lock()).unwrap();

    let output = run_bundle(
        &project,
        &[
            "--frozen",
            "--flake-lock",
            lock.to_str().unwrap(),
            "--out",
            bundle.to_str().unwrap(),
            "--json",
        ],
        &[],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let nix = |args: &[&str]| {
        let output = Command::new("nix")
            .args(args)
            .current_dir(&bundle)
            .env(
                "NIX_CONFIG",
                "experimental-features = nix-command flakes\naccept-flake-config = false",
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "nix command failed: nix {}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        output
    };

    nix(&["flake", "archive", "--no-update-lock-file"]);
    let primed = nix(&[
        "build",
        "--no-update-lock-file",
        "--no-link",
        "--print-out-paths",
        ".#dataset",
    ]);
    nix(&[
        "flake",
        "check",
        "--offline",
        "--no-update-lock-file",
    ]);
    let replay = nix(&[
        "build",
        "--offline",
        "--no-update-lock-file",
        "--no-link",
        "--print-out-paths",
        ".#dataset",
    ]);
    assert_eq!(stdout(&primed).trim(), stdout(&replay).trim());
}
