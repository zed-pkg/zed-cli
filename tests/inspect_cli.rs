use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn zed() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn inspect(root: &Path) -> Output {
    Command::new(zed())
        .args(["inspect", "--format", "json", "--root"])
        .arg(root)
        .output()
        .unwrap()
}

fn entries(root: &Path) -> Vec<OsString> {
    let mut entries = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn codes(report: &Value) -> Vec<&str> {
    report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic["code"].as_str().unwrap())
        .collect()
}

#[test]
fn inspect_reports_git_mise_and_nix_without_mutating_or_running_them() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join(".zpkg.toml"),
        r#"[package]
org = "acme"
name = "static-interop"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/static-interop.git"

[interop]
git-submodules = true
"#,
    )
    .unwrap();
    fs::write(
        project.path().join(".gitmodules"),
        "[submodule \"vendor/example\"]\n\tpath = vendor/example\n\turl = https://example.invalid/example.git\n",
    )
    .unwrap();
    fs::write(project.path().join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
    fs::write(project.path().join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    let before = entries(project.path());

    let output = inspect(project.path());

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(entries(project.path()), before);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], "1.0");
    assert_eq!(report["producer"]["name"], "zed-pkg");
    assert_eq!(report["interop"]["git_submodules"]["enabled"], true);
    assert_eq!(report["interop"]["git_submodules"]["ready"], true);
    assert_eq!(report["interop"]["mise"]["detected"], true);
    assert_eq!(report["interop"]["nix_develop"]["detected"], true);
    let codes = codes(&report);
    assert!(codes.contains(&"MISE_LOCK_MISSING"));
    assert!(codes.contains(&"NIX_LOCK_MISSING"));
}

#[test]
fn inspect_returns_structured_json_for_an_unavailable_root() {
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing-project");

    let output = inspect(&missing);

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["summary"]["health"], "error");
    assert_eq!(codes(&report), ["ROOT_UNAVAILABLE"]);
}

#[test]
fn inspect_rejects_a_non_boolean_git_interop_claim() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join(".zpkg.toml"),
        r#"[package]
org = "acme"
name = "bad-interop"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/bad-interop.git"

[interop]
git-submodules = "yes"
"#,
    )
    .unwrap();
    fs::write(project.path().join(".gitmodules"), "").unwrap();

    let output = inspect(project.path());

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(codes(&report).contains(&"GITMODULES_FLAG_INVALID"));
    assert_eq!(report["interop"]["git_submodules"]["enabled"], false);
}

#[test]
fn inspect_verifies_locked_mise_and_nix_develop_composition() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join("mise.toml"),
        "[settings]\nlockfile = true\n\n[tools]\nnode = \"22\"\n",
    )
    .unwrap();
    fs::write(
        project.path().join("mise.lock"),
        r#"[[tools.node]]
version = "22.4.0"
backend = "core:node"

[tools.node.platforms.linux-x64]
checksum = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
size = 123
url = "https://example.invalid/node.tar.xz"
"#,
    )
    .unwrap();
    fs::write(project.path().join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    fs::write(
        project.path().join("flake.lock"),
        r#"{
  "nodes": {
    "root": { "inputs": {} }
  },
  "root": "root",
  "version": 7
}
"#,
    )
    .unwrap();

    let output = inspect(project.path());

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["interop"]["mise"]["ready"], true);
    assert_eq!(report["interop"]["nix_develop"]["ready"], true);
    assert!(codes(&report).contains(&"ENVIRONMENT_LAYERED"));
}

#[test]
fn inspect_matches_zed_develop_nested_flake_precedence() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir(project.path().join(".nix")).unwrap();
    fs::write(project.path().join("flake.nix"), "{ outputs = _: {}; }\n").unwrap();
    fs::write(
        project.path().join("flake.lock"),
        "{\"nodes\":{\"root\":{}},\"root\":\"root\",\"version\":7}\n",
    )
    .unwrap();
    fs::write(
        project.path().join(".nix/flake.nix"),
        "{ outputs = _: {}; }\n",
    )
    .unwrap();

    let output = inspect(project.path());

    assert!(output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let codes = codes(&report);
    assert!(codes.contains(&"NIX_FLAKE_SHADOWED"));
    assert!(codes.contains(&"NIX_LOCK_MISSING"));
    assert_eq!(report["interop"]["nix_develop"]["ready"], false);
}
