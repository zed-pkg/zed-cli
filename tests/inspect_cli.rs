use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn manifest(interop: Option<bool>) -> String {
    let interop = interop
        .map(|enabled| format!("\n[interop]\ngit-submodules = {enabled}\n"))
        .unwrap_or_default();
    format!(
        r#"[package]
org = "acme"
name = "workspace"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.com/acme/workspace"
{interop}"#
    )
}

fn run_inspect(root: &Path) -> (Value, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(["inspect", "--format", "json", "--root"])
        .arg(root)
        .output()
        .expect("run zed inspect");
    assert!(
        output.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let report = serde_json::from_str(&stdout).expect("one JSON document on stdout");
    (
        report,
        String::from_utf8(output.stderr).expect("UTF-8 stderr"),
    )
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn inspect_is_non_mutating_and_reports_manifest_policy() {
    let project = tempfile::tempdir().unwrap();
    fs::write(project.path().join(".zpkg.toml"), manifest(Some(true))).unwrap();
    let recovery = project.path().join(".zpkg-staging/transaction/state.json");
    fs::create_dir_all(recovery.parent().unwrap()).unwrap();
    fs::write(&recovery, "{}\n").unwrap();

    let (report, stderr) = run_inspect(project.path());

    assert_eq!(report["schema_version"], "1.0");
    assert_eq!(
        report["interop"]["git_submodules"]["manifest_declared"],
        true
    );
    assert_eq!(
        report["interop"]["git_submodules"]["effective_default"],
        true
    );
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["code"] == "GITMODULES_MISSING")
    );
    assert!(stderr.is_empty(), "inspect wrote to stderr: {stderr}");
    assert!(
        recovery.is_file(),
        "inspection must not recover or delete state"
    );
}

#[test]
fn inspect_flags_undeclared_gitmodules_for_every_editor_client() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init", "--quiet"]);
    git(project.path(), &["config", "user.name", "Zed Test"]);
    git(
        project.path(),
        &["config", "user.email", "zed-test@example.invalid"],
    );
    fs::write(project.path().join(".zpkg.toml"), manifest(None)).unwrap();
    fs::write(
        project.path().join(".gitmodules"),
        "[submodule \"vendor/client\"]\n\tpath = vendor/client\n\turl = https://example.com/acme/client.git\n",
    )
    .unwrap();
    git(project.path(), &["add", ".zpkg.toml", ".gitmodules"]);
    git(project.path(), &["commit", "--quiet", "-m", "fixture"]);

    let (report, _) = run_inspect(project.path());
    let codes = report["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["code"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert!(codes.contains(&"GITMODULES_UNDECLARED"));
    assert!(codes.contains(&"GIT_SUBMODULE_CHECKOUT_DRIFT"));
    assert_eq!(
        report["interop"]["git_submodules"]["entries"][0]["path"],
        "vendor/client"
    );
}
