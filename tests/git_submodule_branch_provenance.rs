#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::Command;

use zed_cli::config::Config;
use zed_cli::git_submodules::overtake;

const MANIFEST_FILE: &str = ".zpkg.toml";
const LOCKFILE_FILE: &str = ".zpkg.lock";

fn git(project: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Zed Test")
        .env("GIT_AUTHOR_EMAIL", "zed@example.invalid")
        .env("GIT_COMMITTER_NAME", "Zed Test")
        .env("GIT_COMMITTER_EMAIL", "zed@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output is UTF-8")
        .trim()
        .to_string()
}

fn write_package(project: &Path, org: &str, name: &str, repository: &str) {
    fs::write(
        project.join(MANIFEST_FILE),
        format!(
            r#"[package]
org = "{org}"
name = "{name}"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "{repository}"
"#
        ),
    )
    .expect("write package manifest");
}

fn read_locked_identity(project: &Path) -> (String, Option<String>) {
    let text = fs::read_to_string(project.join(LOCKFILE_FILE)).expect("read lockfile");
    let document: toml::Value = toml::from_str(&text).expect("parse lockfile");
    let entries = document
        .get("git-submodule")
        .and_then(toml::Value::as_array)
        .expect("git-submodule lock extension");
    assert_eq!(entries.len(), 1, "one submodule should be locked");
    let entry = entries[0].as_table().expect("git-submodule table");
    let commit = entry
        .get("commit")
        .and_then(toml::Value::as_str)
        .expect("immutable gitlink commit")
        .to_string();
    let branch = entry
        .get("branch")
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned);
    (commit, branch)
}

#[test]
fn branch_metadata_can_change_without_floating_the_locked_gitlink_commit() {
    let child = tempfile::tempdir().expect("child repository");
    git(child.path(), &["init"]);
    write_package(
        child.path(),
        "acme",
        "client",
        "https://example.invalid/acme/client.git",
    );
    fs::write(child.path().join("lib.txt"), "hello\n").expect("write child source");
    git(child.path(), &["add", "."]);
    git(child.path(), &["commit", "-m", "child"]);
    let child_commit = git(child.path(), &["rev-parse", "HEAD"]);

    let root = tempfile::tempdir().expect("root repository");
    git(root.path(), &["init"]);
    git(root.path(), &["config", "protocol.file.allow", "always"]);
    write_package(
        root.path(),
        "acme",
        "root",
        "https://example.invalid/acme/root.git",
    );
    let child_url = child.path().to_str().expect("UTF-8 child path");
    git(
        root.path(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child_url,
            "vendor/client",
        ],
    );
    git(
        root.path(),
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.vendor/client.branch",
            "stable",
        ],
    );
    git(root.path(), &["add", "."]);
    git(
        root.path(),
        &["commit", "-m", "root with stable transport branch"],
    );

    let home = tempfile::tempdir().expect("Zed home");
    let config = Config {
        registry: "file:///unused".to_string(),
        home: home.path().to_path_buf(),
        token: None,
        auth_url: "http://127.0.0.1/unused".to_string(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
        local: Default::default(),
    };

    let report = overtake(root.path(), &config).expect("initial overtake");
    assert_eq!(report.adopted, 1);
    let (first_commit, first_branch) = read_locked_identity(root.path());
    assert_eq!(first_commit, child_commit);
    assert_eq!(first_branch.as_deref(), Some("stable"));

    git(
        root.path(),
        &[
            "config",
            "--file",
            ".gitmodules",
            "submodule.vendor/client.branch",
            "next",
        ],
    );
    git(root.path(), &["add", ".gitmodules"]);
    git(
        root.path(),
        &["commit", "-m", "change transport branch metadata"],
    );

    let report = overtake(root.path(), &config).expect("refresh overtake");
    assert_eq!(report.adopted, 1);
    let (second_commit, second_branch) = read_locked_identity(root.path());
    assert_eq!(second_branch.as_deref(), Some("next"));
    assert_eq!(
        second_commit, first_commit,
        "branch metadata must never replace the exact superproject gitlink commit"
    );
}
