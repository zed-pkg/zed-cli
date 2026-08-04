use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

use clap::Parser;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};

use super::cli::{OvertakeArgs, OvertakeCli, OvertakeCommand, Route, route};
use super::git::{WorkspaceMember, generated_consumer_manifest, validate_relative_path};
#[cfg(unix)]
use super::lock::prepare_install;
use super::lock::{
    GitSubmoduleLock, active_workspace_packages, read_lock_extensions, write_lock_extensions,
};
use super::*;
#[cfg(unix)]
use crate::config::{Config, read_manifest};

#[cfg(unix)]
fn git(project: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Zed Test")
        .env("GIT_AUTHOR_EMAIL", "zed@example.invalid")
        .env("GIT_COMMITTER_NAME", "Zed Test")
        .env("GIT_COMMITTER_EMAIL", "zed@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

#[cfg(unix)]
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
    .unwrap();
}

#[test]
fn route_recognizes_overtake_and_help_with_global_options() {
    let words = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
    assert_eq!(
        route(&words(&["zed", "overtake", "--git-submodules"])),
        Route::Overtake
    );
    assert!(matches!(
        route(&words(&[
            "zed",
            "--home",
            "/tmp/zed-home",
            "help",
            "overtake"
        ])),
        Route::OvertakeHelp { .. }
    ));
}

#[test]
fn boolish_overtake_flag_supports_bare_and_explicit_off() {
    let cli = OvertakeCli::try_parse_from(["zed", "overtake", "--git-submodules"]).unwrap();
    assert!(matches!(
        cli.command,
        OvertakeCommand::Overtake(OvertakeArgs {
            git_submodules: true
        })
    ));
    let cli = OvertakeCli::try_parse_from(["zed", "overtake", "--git-submodules=false"]).unwrap();
    assert!(matches!(
        cli.command,
        OvertakeCommand::Overtake(OvertakeArgs {
            git_submodules: false
        })
    ));
}

#[test]
fn submodule_paths_reject_nested_git_and_transaction_components() {
    for path in [
        "vendor/.git/hooks",
        "vendor/.GIT/hooks",
        "vendor/../escape",
        "vendor/./client",
        "vendor//client",
    ] {
        assert!(
            validate_relative_path(path).is_err(),
            "unsafe path unexpectedly accepted: {path}"
        );
    }
    let transaction_path = format!("vendor/{}/state", crate::transaction::STAGING_DIR);
    assert!(validate_relative_path(&transaction_path).is_err());
    validate_relative_path("vendor/client").unwrap();
}

#[test]
fn adopted_workspace_reachability_includes_build_dependencies_transitively() {
    let root_dir = tempfile::tempdir().unwrap();
    let tool_dir = tempfile::tempdir().unwrap();
    let codegen_dir = tempfile::tempdir().unwrap();

    let mut root = generated_consumer_manifest(root_dir.path());
    root.build_dependencies
        .insert("acme/tool".to_string(), "=1.2.3".to_string());

    let mut tool = generated_consumer_manifest(tool_dir.path());
    tool.package.org = "acme".to_string();
    tool.package.name = "tool".to_string();
    tool.build_dependencies
        .insert("acme/codegen".to_string(), "=1.2.3".to_string());

    let mut codegen = generated_consumer_manifest(codegen_dir.path());
    codegen.package.org = "acme".to_string();
    codegen.package.name = "codegen".to_string();

    let members = BTreeMap::from([
        (
            "acme/tool".to_string(),
            WorkspaceMember {
                path: "vendor/tool".to_string(),
                root: tool_dir.path().to_path_buf(),
                manifest: tool,
            },
        ),
        (
            "acme/codegen".to_string(),
            WorkspaceMember {
                path: "vendor/codegen".to_string(),
                root: codegen_dir.path().to_path_buf(),
                manifest: codegen,
            },
        ),
    ]);

    assert_eq!(
        active_workspace_packages(&root, &members),
        ["acme/codegen".to_string(), "acme/tool".to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn additive_lock_tables_remain_readable_by_canonical_parser() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join(LOCKFILE_FILE), "version = 1\n").unwrap();
    let entry = GitSubmoduleLock {
        name: "client".to_string(),
        path: "vendor/client".to_string(),
        package: "acme/client".to_string(),
        version: "1.2.3".to_string(),
        url: "https://example.invalid/acme/client.git".to_string(),
        commit: "0123456789abcdef0123456789abcdef01234567".to_string(),
        sha256: "1".repeat(64),
        size: 10,
        format: ArtifactFormat::TarGz,
        branch: None,
    };
    write_lock_extensions(dir.path(), std::slice::from_ref(&entry)).unwrap();
    let text = fs::read_to_string(dir.path().join(LOCKFILE_FILE)).unwrap();
    assert!(text.contains("[[git-submodule]]"));
    Lockfile::parse(&text).unwrap();
    assert_eq!(read_lock_extensions(dir.path()).unwrap(), [entry]);
}

#[test]
fn empty_extension_refresh_preserves_standard_lock_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let original = b"# keep formatting\nversion = 1\n";
    fs::write(dir.path().join(LOCKFILE_FILE), original).unwrap();
    write_lock_extensions(dir.path(), &[]).unwrap();
    assert_eq!(fs::read(dir.path().join(LOCKFILE_FILE)).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn overtake_imports_manifest_workspace_and_git_lock() {
    let child = tempfile::tempdir().unwrap();
    git(child.path(), &["init"]);
    write_package(
        child.path(),
        "acme",
        "client",
        "https://example.invalid/acme/client.git",
    );
    fs::write(child.path().join("lib.txt"), "hello\n").unwrap();
    git(child.path(), &["add", "."]);
    git(child.path(), &["commit", "-m", "child"]);

    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init"]);
    git(root.path(), &["config", "protocol.file.allow", "always"]);
    write_package(
        root.path(),
        "acme",
        "root",
        "https://example.invalid/acme/root.git",
    );
    let child_url = child.path().to_str().unwrap();
    let status = Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child_url,
            "vendor/client",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap();
    assert!(status.success());
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-m", "root"]);
    git(
        root.path(),
        &["submodule", "deinit", "--force", "--", "vendor/client"],
    );

    let home = tempfile::tempdir().unwrap();
    let cfg = Config {
        registry: "file:///unused".to_string(),
        home: home.path().to_path_buf(),
        token: None,
        auth_url: "http://127.0.0.1/unused".to_string(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
    };
    let report = overtake(root.path(), &cfg).unwrap();
    assert_eq!(report.adopted, 1);

    let manifest = read_manifest(root.path()).unwrap();
    assert_eq!(
        manifest.dependencies.get("acme/client").map(String::as_str),
        Some("=1.2.3")
    );
    assert!(
        manifest
            .workspace
            .as_ref()
            .unwrap()
            .members
            .contains(&"vendor/client".to_string())
    );
    let entries = read_lock_extensions(root.path()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].package, "acme/client");
    assert!(root.path().join("zed_modules/acme/client/lib.txt").exists());
    assert!(matches!(
        prepare_install(root.path(), true).unwrap(),
        super::lock::InstallLockPlan::Frozen
    ));

    let gitmodules_path = root.path().join(".gitmodules");
    let gitmodules = fs::read_to_string(&gitmodules_path).unwrap();
    assert!(gitmodules.contains(child_url));
    fs::write(
        &gitmodules_path,
        gitmodules.replace(child_url, "https://example.invalid/acme/client-mirror.git"),
    )
    .unwrap();
    git(root.path(), &["add", ".gitmodules"]);
    git(root.path(), &["commit", "-m", "change submodule transport"]);

    let error = prepare_install(root.path(), true).unwrap_err();
    assert!(
        format!("{error:#}").contains("url"),
        "unexpected frozen drift error: {error:#}"
    );
}

#[cfg(unix)]
#[test]
fn overtake_install_failure_restores_original_root_manifest() {
    let child = tempfile::tempdir().unwrap();
    git(child.path(), &["init"]);
    write_package(
        child.path(),
        "acme",
        "client",
        "https://example.invalid/acme/client.git",
    );
    let child_manifest_path = child.path().join(MANIFEST_FILE);
    let mut child_manifest = fs::read_to_string(&child_manifest_path).unwrap();
    child_manifest.push_str("\n[dependencies]\n\"acme/missing\" = \"=1.0.0\"\n");
    fs::write(&child_manifest_path, child_manifest).unwrap();
    fs::write(child.path().join("lib.txt"), "hello\n").unwrap();
    git(child.path(), &["add", "."]);
    git(
        child.path(),
        &["commit", "-m", "child with missing dependency"],
    );

    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init"]);
    git(root.path(), &["config", "protocol.file.allow", "always"]);
    let original_manifest = br#"# keep this exact formatting on rollback
[package]
org = "acme"
name = "root"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/root.git"
"#;
    fs::write(root.path().join(MANIFEST_FILE), original_manifest).unwrap();

    let child_url = child.path().to_str().unwrap();
    let status = Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child_url,
            "vendor/client",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap();
    assert!(status.success());
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-m", "root"]);

    let home = tempfile::tempdir().unwrap();
    let cfg = Config {
        registry: "file:///unused".to_string(),
        home: home.path().to_path_buf(),
        token: None,
        auth_url: "http://127.0.0.1/unused".to_string(),
        supabase_url: None,
        supabase_key: None,
        interactive: false,
    };
    let error = overtake(root.path(), &cfg).unwrap_err();
    assert!(
        format!("{error:#}").contains("restored the prior root manifest"),
        "unexpected takeover failure: {error:#}"
    );
    assert_eq!(
        fs::read(root.path().join(MANIFEST_FILE)).unwrap(),
        original_manifest
    );
    assert!(!root.path().join(LOCKFILE_FILE).exists());
    assert!(!root.path().join("zed_modules").exists());
    assert!(!root.path().join(crate::transaction::STAGING_DIR).exists());
}

#[test]
fn failed_overtake_restores_exact_prior_manifest_bytes() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(MANIFEST_FILE);
    let previous = b"# preserve hand formatting\n[package]\nname = \"before\"\n";
    let adopted = b"[package]\nname = \"after\"\n";
    fs::write(&path, adopted).unwrap();

    restore_manifest_if_unchanged(project.path(), &path, adopted, Some(previous)).unwrap();

    assert_eq!(fs::read(&path).unwrap(), previous);
    assert!(
        !project
            .path()
            .join(crate::transaction::STAGING_DIR)
            .exists()
    );
}

#[test]
fn failed_overtake_removes_a_new_generated_manifest() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(MANIFEST_FILE);
    let adopted = b"[package]\nname = \"generated\"\n";
    fs::write(&path, adopted).unwrap();

    restore_manifest_if_unchanged(project.path(), &path, adopted, None).unwrap();

    assert!(!path.exists());
    assert!(
        !project
            .path()
            .join(crate::transaction::STAGING_DIR)
            .exists()
    );
}

#[test]
fn takeover_plan_refuses_a_manifest_changed_by_another_writer() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(MANIFEST_FILE);
    fs::write(&path, b"after").unwrap();

    let error = ensure_manifest_unchanged(&path, Some(b"before")).unwrap_err();

    assert!(error.to_string().contains("another writer"));
    assert_eq!(fs::read(&path).unwrap().as_slice(), b"after");
}

#[test]
fn failed_overtake_never_overwrites_a_concurrent_manifest_edit() {
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(MANIFEST_FILE);
    let adopted = b"[package]\nname = \"adopted\"\n";
    fs::write(&path, b"[package]\nname = \"concurrent\"\n").unwrap();

    let error = restore_manifest_if_unchanged(
        project.path(),
        &path,
        adopted,
        Some(b"[package]\nname = \"before\"\n"),
    )
    .unwrap_err();

    assert!(error.to_string().contains("another writer"));
    assert!(fs::read_to_string(&path).unwrap().contains("concurrent"));
}

#[test]
fn git_lock_finalize_context_marks_a_post_commit_install_error() {
    let post_commit = anyhow::Error::new(crate::ops::GitLockFinalizeError);
    assert!(install_committed_before_error(&post_commit));
    assert!(!install_committed_before_error(&anyhow::anyhow!(
        "resolution failed"
    )));
}
