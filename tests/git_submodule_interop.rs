#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn git(project: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Zed Interop Test")
        .env("GIT_AUTHOR_EMAIL", "zed-interop@example.invalid")
        .env("GIT_COMMITTER_NAME", "Zed Interop Test")
        .env("GIT_COMMITTER_EMAIL", "zed-interop@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
}

fn git_ok(project: &Path, args: &[&str]) {
    let output = git(project, args);
    assert!(
        output.status.success(),
        "git -C {} {} failed\nstdout: {}\nstderr: {}",
        project.display(),
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(project: &Path) {
    fs::create_dir_all(project).unwrap();
    git_ok(project, &["init"]);
}

fn write_package(project: &Path, org: &str, name: &str) {
    fs::write(
        project.join(".zpkg.toml"),
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

fn add_submodule(root: &Path, source: &Path, destination: &str) {
    let source = source.to_str().unwrap();
    git_ok(
        root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source,
            destination,
        ],
    );
}

fn run_zed(project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(zed_bin())
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ZED_PKG_HOME", home.join(".zed-pkg"))
        .env("ZED_PKG_REGISTRY", "file:///unused")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("ZED_PKG_GIT_SUBMODULES")
        .output()
        .unwrap()
}

#[test]
fn mixed_submodules_overtake_and_replay_from_a_fresh_clone() {
    let temp = tempfile::tempdir().unwrap();
    let zed_child = temp.path().join("zed-child");
    let legacy_child = temp.path().join("legacy-child");
    let root = temp.path().join("root");
    let home = temp.path().join("home");
    let fresh_home = temp.path().join("fresh-home");
    let fresh = temp.path().join("fresh");

    init_repo(&zed_child);
    write_package(&zed_child, "acme", "client");
    fs::write(zed_child.join("lib.txt"), "zed package\n").unwrap();
    git_ok(&zed_child, &["add", "."]);
    git_ok(&zed_child, &["commit", "-m", "zed child"]);

    init_repo(&legacy_child);
    fs::write(legacy_child.join("README.md"), "ordinary Git submodule\n").unwrap();
    git_ok(&legacy_child, &["add", "."]);
    git_ok(&legacy_child, &["commit", "-m", "legacy child"]);

    init_repo(&root);
    git_ok(&root, &["config", "protocol.file.allow", "always"]);
    write_package(&root, "acme", "root");
    add_submodule(&root, &zed_child, "vendor/client");
    add_submodule(&root, &legacy_child, "vendor/legacy");
    git_ok(&root, &["add", "."]);
    git_ok(&root, &["commit", "-m", "root with mixed submodules"]);

    fs::create_dir_all(&home).unwrap();
    let takeover = run_zed(&root, &home, &["overtake", "--git-submodules"]);
    assert!(
        takeover.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&takeover.stdout),
        String::from_utf8_lossy(&takeover.stderr)
    );
    let takeover_stdout = String::from_utf8_lossy(&takeover.stdout);
    let takeover_stderr = String::from_utf8_lossy(&takeover.stderr);
    assert!(takeover_stdout.contains("overtook 1 Git submodule package(s)"));
    assert!(takeover_stdout.contains("left 1 non-Zed submodule(s) under Git authority"));
    assert!(takeover_stderr.contains("leaving non-Zed submodule"));
    assert!(takeover_stderr.contains("vendor/legacy"));

    let manifest = fs::read_to_string(root.join(".zpkg.toml")).unwrap();
    assert!(manifest.contains("acme/client"));
    assert!(manifest.contains("vendor/client"));
    assert!(!manifest.contains("vendor/legacy"));

    let lock = fs::read_to_string(root.join(".zpkg.lock")).unwrap();
    assert!(lock.contains("[[git-submodule]]"));
    assert!(lock.contains("package = \"acme/client\""));
    assert!(lock.contains("path = \"vendor/client\""));
    assert!(!lock.contains("vendor/legacy"));

    git_ok(&root, &["add", ".zpkg.toml", ".zpkg.lock"]);
    git_ok(&root, &["commit", "-m", "adopt Zed submodule"]);

    let root_source = root.to_str().unwrap();
    let clone = Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "clone",
            "--no-recurse-submodules",
            root_source,
        ])
        .arg(&fresh)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        clone.status.success(),
        "clone stdout: {}\nclone stderr: {}",
        String::from_utf8_lossy(&clone.stdout),
        String::from_utf8_lossy(&clone.stderr)
    );
    git_ok(&fresh, &["config", "protocol.file.allow", "always"]);
    assert!(!fresh.join("vendor/client/lib.txt").exists());
    assert!(!fresh.join("vendor/legacy/README.md").exists());

    let manifest_before = fs::read(fresh.join(".zpkg.toml")).unwrap();
    let lock_before = fs::read(fresh.join(".zpkg.lock")).unwrap();
    fs::create_dir_all(&fresh_home).unwrap();
    let replay = run_zed(
        &fresh,
        &fresh_home,
        &["install", "--git-submodules", "--frozen"],
    );
    assert!(
        replay.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );

    assert_eq!(fs::read(fresh.join(".zpkg.toml")).unwrap(), manifest_before);
    assert_eq!(fs::read(fresh.join(".zpkg.lock")).unwrap(), lock_before);
    assert_eq!(
        fs::read_to_string(fresh.join("vendor/client/lib.txt")).unwrap(),
        "zed package\n"
    );
    assert_eq!(
        fs::read_to_string(fresh.join("vendor/legacy/README.md")).unwrap(),
        "ordinary Git submodule\n"
    );
    assert_eq!(
        fs::read_to_string(fresh.join("zed_modules/acme/client/lib.txt")).unwrap(),
        "zed package\n"
    );
}

#[test]
fn git_only_takeover_preserves_all_zed_managed_state() {
    let temp = tempfile::tempdir().unwrap();
    let legacy_child = temp.path().join("legacy-child");
    let root = temp.path().join("root");
    let home = temp.path().join("home");

    init_repo(&legacy_child);
    fs::write(legacy_child.join("README.md"), "ordinary Git submodule\n").unwrap();
    git_ok(&legacy_child, &["add", "."]);
    git_ok(&legacy_child, &["commit", "-m", "legacy child"]);

    init_repo(&root);
    git_ok(&root, &["config", "protocol.file.allow", "always"]);
    write_package(&root, "acme", "root");
    add_submodule(&root, &legacy_child, "vendor/legacy");
    git_ok(&root, &["add", "."]);
    git_ok(&root, &["commit", "-m", "root with Git-only submodule"]);
    git_ok(
        &root,
        &[
            "submodule",
            "deinit",
            "--force",
            "--",
            "vendor/legacy",
        ],
    );
    assert!(!root.join("vendor/legacy/README.md").exists());

    let manifest_before = fs::read(root.join(".zpkg.toml")).unwrap();
    fs::create_dir_all(&home).unwrap();
    let takeover = run_zed(&root, &home, &["overtake", "--git-submodules"]);
    assert!(!takeover.status.success());
    let stderr = String::from_utf8_lossy(&takeover.stderr);
    assert!(stderr.contains("no overtake-compatible Zed submodules"));
    assert!(stderr.contains("vendor/legacy"));

    // Cooperative Git synchronization happens before package discovery, but
    // the failed authority migration must not publish any Zed-managed state.
    assert_eq!(fs::read(root.join(".zpkg.toml")).unwrap(), manifest_before);
    assert!(!root.join(".zpkg.lock").exists());
    assert!(!root.join("zed_modules").exists());
    assert!(!root.join(".zpkg-staging").exists());
    assert_eq!(
        fs::read_to_string(root.join("vendor/legacy/README.md")).unwrap(),
        "ordinary Git submodule\n"
    );
}
