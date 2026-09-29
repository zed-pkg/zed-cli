use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn zed() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(zed());
    command.current_dir(root);
    for key in [
        "ZED_PKG_HOME",
        "ZED_PKG_TOKEN",
        "ZED_PKG_INTERACTIVE",
        "ZED_PKG_GIT_SUBMODULES",
        "ZED_PKG_VALIDATE_MANIFEST",
        "ZED_PKG_VALIDATE_LOCK",
        "ZED_PKG_VALIDATE_REQUIRE_LOCK",
        "ZED_PKG_VALIDATE_JSON",
    ] {
        command.env_remove(key);
    }
    command.env("ZED_PKG_HOME", root.join("home-must-not-be-created"));
    return command;
}

fn run(root: &Path, args: &[&str]) -> Output {
    return command(root).args(args).output().expect("run zed validate");
}

fn zero_dependency_project(with_lock: bool) -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("temp project");
    fs::write(
        project.path().join(".zpkg.toml"),
        r#"[package]
org = "acme"
name = "zero-deps"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://github.com/acme/zero-deps"
"#,
    )
    .expect("write manifest");
    if with_lock {
        fs::write(project.path().join(".zpkg.lock"), "version = 1\n").expect("write lock");
    }
    return project;
}

#[test]
fn zero_dependency_lock_has_no_transitive_warning_in_human_or_json_output() {
    let project = zero_dependency_project(true);

    let human = run(project.path(), &["validate", "--require-lock"]);
    assert!(
        human.status.success(),
        "{}",
        String::from_utf8_lossy(&human.stderr)
    );
    let human_stdout = String::from_utf8_lossy(&human.stdout);
    assert!(human_stdout.contains("checked 0 direct dependency requirement(s)"));
    assert!(!human_stdout.contains("warning:"), "{human_stdout}");

    let json = run(project.path(), &["validate", "--require-lock", "--json"]);
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).expect("validation json");
    assert_eq!(report["manifest"]["direct_requirements"], 0);
    assert_eq!(report["direct_requirements_checked"], 0);
    assert_eq!(report["warnings"], serde_json::json!([]));
}

#[test]
fn optional_absent_lock_is_quiet_when_manifest_has_no_dependency_roots() {
    let project = zero_dependency_project(false);
    let output = run(project.path(), &["validate"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("lockfile not present: .zpkg.lock (not required)"));
    assert!(!stdout.contains("warning:"), "{stdout}");
}
