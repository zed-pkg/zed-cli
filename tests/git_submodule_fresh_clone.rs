#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const MANIFEST_FILE: &str = ".zpkg.toml";
const LOCKFILE_FILE: &str = ".zpkg.lock";
const SUBMODULE_PATH: &str = "vendor/client";

fn run(command: &mut Command, operation: &str) -> Output {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{operation} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(project: &Path, args: &[&str]) -> Output {
    run(
        Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Zed Test")
            .env("GIT_AUTHOR_EMAIL", "zed@example.invalid")
            .env("GIT_COMMITTER_NAME", "Zed Test")
            .env("GIT_COMMITTER_EMAIL", "zed@example.invalid")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ALLOW_PROTOCOL", "file"),
        &format!("git -C {} {}", project.display(), args.join(" ")),
    )
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

fn add_submodule(project: &Path, source: &Path) {
    git(
        project,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source.to_str().unwrap(),
            SUBMODULE_PATH,
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
        .env_remove("ZED_PKG_ALLOW_BUILD");
    command
}

#[test]
fn frozen_install_restores_an_overtaken_graph_from_a_fresh_clone() {
    let child = tempfile::tempdir().unwrap();
    init_repository(child.path());
    write_package(child.path(), "acme", "client");
    fs::write(child.path().join("payload.txt"), "client payload\n").unwrap();
    commit_all(child.path(), "client");

    let root = tempfile::tempdir().unwrap();
    init_repository(root.path());
    write_package(root.path(), "acme", "root");
    add_submodule(root.path(), child.path());
    commit_all(root.path(), "root with Git transport");

    let author_home = tempfile::tempdir().unwrap();
    run(
        zed(root.path(), author_home.path()).args(["overtake", "--git-submodules"]),
        "zed overtake --git-submodules",
    );
    let expected_manifest = fs::read(root.path().join(MANIFEST_FILE)).unwrap();
    let expected_lock = fs::read(root.path().join(LOCKFILE_FILE)).unwrap();
    git(root.path(), &["add", MANIFEST_FILE, LOCKFILE_FILE]);
    git(
        root.path(),
        &["commit", "-m", "adopt submodule into the Zed graph"],
    );

    let clone_parent = tempfile::tempdir().unwrap();
    let fresh = clone_parent.path().join("fresh");
    git(
        clone_parent.path(),
        &[
            "clone",
            "--no-recurse-submodules",
            root.path().to_str().unwrap(),
            fresh.to_str().unwrap(),
        ],
    );
    assert!(fresh.join(".gitmodules").is_file());
    assert!(!fresh.join("vendor/client/payload.txt").exists());
    assert!(!fresh.join("zed_modules/acme/client/payload.txt").exists());

    let consumer_home = tempfile::tempdir().unwrap();
    run(
        zed(&fresh, consumer_home.path())
            .env("ZED_PKG_GIT_SUBMODULES", "1")
            .args(["install", "--frozen"]),
        "fresh-clone frozen install with Git-submodule transport",
    );

    assert!(fresh.join("vendor/client/payload.txt").is_file());
    assert!(fresh.join("zed_modules/acme/client/payload.txt").is_file());
    assert_eq!(fs::read(fresh.join(MANIFEST_FILE)).unwrap(), expected_manifest);
    assert_eq!(fs::read(fresh.join(LOCKFILE_FILE)).unwrap(), expected_lock);

    let status = String::from_utf8(git(&fresh, &["submodule", "status", "--recursive"]).stdout)
        .unwrap();
    assert!(
        status
            .lines()
            .filter(|line| !line.trim().is_empty())
            .all(|line| !matches!(line.as_bytes().first(), Some(b'-' | b'+' | b'U'))),
        "fresh clone retained submodule drift: {status}"
    );
}
