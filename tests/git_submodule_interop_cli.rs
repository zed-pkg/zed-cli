#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const MANIFEST_FILE: &str = ".zpkg.toml";
const LOCKFILE_FILE: &str = ".zpkg.lock";
const SUBMODULE_PATH: &str = "vendor/client";
const SUBMODULE_PAYLOAD: &str = "vendor/client/payload.txt";
const NESTED_PAYLOAD: &str = "vendor/client/vendor/leaf/leaf.txt";

fn git(project: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Zed Test")
        .env("GIT_AUTHOR_EMAIL", "zed@example.invalid")
        .env("GIT_COMMITTER_NAME", "Zed Test")
        .env("GIT_COMMITTER_EMAIL", "zed@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap();
    assert_success(
        &output,
        &format!("git -C {} {}", project.display(), args.join(" ")),
    );
    output
}

fn git_text(project: &Path, args: &[&str]) -> String {
    String::from_utf8(git(project, args).stdout).unwrap()
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output, operation: &str) {
    assert!(
        !output.status.success(),
        "{operation} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repository(project: &Path) {
    git(project, &["init"]);
    git(project, &["config", "protocol.file.allow", "always"]);
}

fn commit_all(project: &Path, message: &str) {
    git(project, &["add", "."]);
    git(project, &["commit", "-m", message]);
}

fn write_package(project: &Path, org: &str, name: &str) {
    fs::write(
        project.join(MANIFEST_FILE),
        format!(
            r#"[package]
org = "{org}"
name = "{name}"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/{org}/{name}.git"
"#
        ),
    )
    .unwrap();
}

fn add_submodule(project: &Path, source: &Path, path: &str) {
    let source = source.to_str().unwrap();
    git(
        project,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source,
            path,
        ],
    );
}

fn zed(project: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command
        .current_dir(project)
        .env("ZED_PKG_HOME", home)
        .env("ZED_PKG_REGISTRY", "file:///unused")
        .env("ZED_PKG_AUTH_URL", "http://127.0.0.1/unused")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env_remove("ZED_PKG_GIT_SUBMODULES")
        .env_remove("ZED_PKG_INTERACTIVE")
        .env_remove("ZED_PKG_FROZEN")
        .env_remove("ZED_PKG_ALLOW_BUILD");
    command
}

fn deinit_client(project: &Path) {
    git(
        project,
        &["submodule", "deinit", "--force", "--", SUBMODULE_PATH],
    );
    assert!(!project.join(SUBMODULE_PAYLOAD).exists());
}

#[test]
fn install_transport_is_recursive_and_obeys_cli_environment_precedence() {
    let leaf = tempfile::tempdir().unwrap();
    init_repository(leaf.path());
    fs::write(leaf.path().join("leaf.txt"), "nested\n").unwrap();
    commit_all(leaf.path(), "leaf");

    let child = tempfile::tempdir().unwrap();
    init_repository(child.path());
    write_package(child.path(), "acme", "client");
    fs::write(child.path().join("payload.txt"), "client\n").unwrap();
    add_submodule(child.path(), leaf.path(), "vendor/leaf");
    commit_all(child.path(), "client with nested transport");

    let root = tempfile::tempdir().unwrap();
    init_repository(root.path());
    write_package(root.path(), "acme", "root");
    add_submodule(root.path(), child.path(), SUBMODULE_PATH);
    commit_all(root.path(), "root");
    deinit_client(root.path());

    let home = tempfile::tempdir().unwrap();

    let default_off = zed(root.path(), home.path())
        .arg("install")
        .output()
        .unwrap();
    assert_success(
        &default_off,
        "zed install with submodule transport disabled by default",
    );
    assert!(!root.path().join(SUBMODULE_PAYLOAD).exists());

    let env_on = zed(root.path(), home.path())
        .env("ZED_PKG_GIT_SUBMODULES", "ON")
        .arg("install")
        .output()
        .unwrap();
    assert_success(&env_on, "ZED_PKG_GIT_SUBMODULES=ON zed install");
    assert!(root.path().join(SUBMODULE_PAYLOAD).is_file());
    assert!(root.path().join(NESTED_PAYLOAD).is_file());

    deinit_client(root.path());
    let explicit_off = zed(root.path(), home.path())
        .env("ZED_PKG_GIT_SUBMODULES", "definitely-invalid")
        .args(["install", "--git-submodules=false"])
        .output()
        .unwrap();
    assert_success(
        &explicit_off,
        "explicit --git-submodules=false overriding an invalid inherited value",
    );
    assert!(!root.path().join(SUBMODULE_PAYLOAD).exists());

    let invalid_env = zed(root.path(), home.path())
        .env("ZED_PKG_GIT_SUBMODULES", "definitely-invalid")
        .arg("install")
        .output()
        .unwrap();
    assert_failure(&invalid_env, "invalid Git-submodule environment value");
    assert_eq!(invalid_env.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid_env.stderr).contains("ZED_PKG_GIT_SUBMODULES"));
    assert!(!root.path().join(SUBMODULE_PAYLOAD).exists());

    let explicit_on = zed(root.path(), home.path())
        .env("ZED_PKG_GIT_SUBMODULES", "0")
        .args(["--git-submodules", "install"])
        .output()
        .unwrap();
    assert_success(
        &explicit_on,
        "global --git-submodules overriding an inherited false value",
    );
    assert!(root.path().join(SUBMODULE_PAYLOAD).is_file());
    assert!(root.path().join(NESTED_PAYLOAD).is_file());
}

#[test]
fn overtake_is_idempotent_and_keeps_git_as_an_unchanged_transport_mirror() {
    let child = tempfile::tempdir().unwrap();
    init_repository(child.path());
    write_package(child.path(), "acme", "client");
    fs::write(child.path().join("payload.txt"), "client\n").unwrap();
    commit_all(child.path(), "client");

    let root = tempfile::tempdir().unwrap();
    init_repository(root.path());
    write_package(root.path(), "acme", "root");
    add_submodule(root.path(), child.path(), SUBMODULE_PATH);
    commit_all(root.path(), "root");

    let gitmodules_before = fs::read(root.path().join(".gitmodules")).unwrap();
    let gitlink_before = git_text(root.path(), &["ls-tree", "HEAD", "--", SUBMODULE_PATH]);
    let home = tempfile::tempdir().unwrap();

    let first = zed(root.path(), home.path())
        .args(["--git-submodules", "overtake"])
        .output()
        .unwrap();
    assert_success(&first, "first zed --git-submodules overtake");

    let manifest_once = fs::read(root.path().join(MANIFEST_FILE)).unwrap();
    let lock_once = fs::read(root.path().join(LOCKFILE_FILE)).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&manifest_once)
            .matches("vendor/client")
            .count(),
        1
    );
    assert_eq!(
        String::from_utf8_lossy(&lock_once)
            .matches("[[git-submodule]]")
            .count(),
        1
    );

    let second = zed(root.path(), home.path())
        .env("ZED_PKG_GIT_SUBMODULES", "yes")
        .arg("overtake")
        .output()
        .unwrap();
    assert_success(&second, "environment-enabled second zed overtake");

    assert_eq!(
        fs::read(root.path().join(MANIFEST_FILE)).unwrap(),
        manifest_once
    );
    assert_eq!(
        fs::read(root.path().join(LOCKFILE_FILE)).unwrap(),
        lock_once
    );
    assert_eq!(
        fs::read(root.path().join(".gitmodules")).unwrap(),
        gitmodules_before
    );
    assert_eq!(
        git_text(root.path(), &["ls-tree", "HEAD", "--", SUBMODULE_PATH]),
        gitlink_before
    );

    let explicitly_disabled = zed(root.path(), home.path())
        .env("ZED_PKG_GIT_SUBMODULES", "true")
        .args(["overtake", "--git-submodules=false"])
        .output()
        .unwrap();
    assert_failure(
        &explicitly_disabled,
        "explicitly disabled Git-submodule takeover",
    );
    assert!(
        String::from_utf8_lossy(&explicitly_disabled.stderr)
            .contains("no takeover source selected")
    );
    assert_eq!(
        fs::read(root.path().join(MANIFEST_FILE)).unwrap(),
        manifest_once
    );
    assert_eq!(
        fs::read(root.path().join(LOCKFILE_FILE)).unwrap(),
        lock_once
    );
}

#[test]
fn failed_overtake_without_a_root_manifest_leaves_no_partial_zed_state() {
    let child = tempfile::tempdir().unwrap();
    init_repository(child.path());
    write_package(child.path(), "acme", "client");
    let manifest_path = child.path().join(MANIFEST_FILE);
    let mut manifest = fs::read_to_string(&manifest_path).unwrap();
    manifest.push_str("\n[dependencies]\n\"acme/missing\" = \"=1.0.0\"\n");
    fs::write(&manifest_path, manifest).unwrap();
    fs::write(child.path().join("payload.txt"), "client\n").unwrap();
    commit_all(child.path(), "client with unresolved dependency");

    let root = tempfile::tempdir().unwrap();
    init_repository(root.path());
    add_submodule(root.path(), child.path(), SUBMODULE_PATH);
    commit_all(root.path(), "manifestless root");

    let gitmodules_before = fs::read(root.path().join(".gitmodules")).unwrap();
    let gitlink_before = git_text(root.path(), &["ls-tree", "HEAD", "--", SUBMODULE_PATH]);
    let home = tempfile::tempdir().unwrap();

    let failed = zed(root.path(), home.path())
        .args(["overtake", "--git-submodules"])
        .output()
        .unwrap();
    assert_failure(
        &failed,
        "manifestless takeover with an unresolved dependency",
    );
    assert!(String::from_utf8_lossy(&failed.stderr).contains("restored the prior root manifest"));

    assert!(!root.path().join(MANIFEST_FILE).exists());
    assert!(!root.path().join(LOCKFILE_FILE).exists());
    assert!(!root.path().join("zed_modules").exists());
    assert!(!root.path().join(zed_cli::transaction::STAGING_DIR).exists());
    assert_eq!(
        fs::read(root.path().join(".gitmodules")).unwrap(),
        gitmodules_before
    );
    assert_eq!(
        git_text(root.path(), &["ls-tree", "HEAD", "--", SUBMODULE_PATH]),
        gitlink_before
    );
}
