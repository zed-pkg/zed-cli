#[cfg(unix)]
use std::process::Command;

use zed_interfaces::manifest::Manifest;

use super::*;

fn manifest_text(extra: &str) -> String {
    format!(
        r#"[package]
org = "acme"
name = "ignored-input-guard"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/ignored-input-guard.git"

{extra}
"#
    )
}

#[cfg(unix)]
fn run_git(project: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Zed Input Guard")
        .env("GIT_AUTHOR_EMAIL", "zed-input-guard@example.invalid")
        .env("GIT_COMMITTER_NAME", "Zed Input Guard")
        .env("GIT_COMMITTER_EMAIL", "zed-input-guard@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

#[cfg(unix)]
fn fixture(manifest_extra: &str) -> (tempfile::TempDir, Manifest) {
    let project = tempfile::tempdir().unwrap();
    run_git(project.path(), &["init"]);
    let source = manifest_text(manifest_extra);
    fs::write(project.path().join(".zpkg.toml"), &source).unwrap();
    fs::write(project.path().join(".gitignore"), "private.env\n").unwrap();
    fs::create_dir_all(project.path().join("src")).unwrap();
    fs::write(project.path().join("src/lib.txt"), "runtime\n").unwrap();
    fs::write(project.path().join("private.env"), "TOKEN=secret\n").unwrap();
    run_git(
        project.path(),
        &["add", ".zpkg.toml", ".gitignore", "src/lib.txt"],
    );
    run_git(project.path(), &["commit", "-m", "fixture"]);
    let manifest = harden_manifest(Manifest::parse(&source).unwrap());
    (project, manifest)
}

#[test]
fn non_git_package_keeps_manifest_only_packaging_behavior() {
    let project = tempfile::tempdir().unwrap();
    let source = manifest_text("");
    fs::write(project.path().join(".zpkg.toml"), &source).unwrap();
    fs::write(project.path().join("local.txt"), "runtime\n").unwrap();
    let manifest = harden_manifest(Manifest::parse(&source).unwrap());
    assert_eq!(
        preflight_ignored_inputs(project.path(), &manifest).unwrap(),
        0
    );
}

#[test]
fn hardening_excludes_the_allowlist_control_file() {
    let manifest = harden_manifest(Manifest::parse(&manifest_text("")).unwrap());
    assert!(
        manifest
            .publish
            .exclude
            .iter()
            .any(|pattern| pattern == IGNORED_INPUT_ALLOW_FILE)
    );
}

#[cfg(unix)]
#[test]
fn ignored_untracked_file_that_would_ship_is_rejected() {
    let (project, manifest) = fixture("");
    let error = preflight_ignored_inputs(project.path(), &manifest).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("private.env"), "{message}");
    assert!(message.contains(IGNORED_INPUT_ALLOW_FILE), "{message}");
}

#[cfg(unix)]
#[test]
fn manifest_exclusion_makes_an_ignored_file_safe() {
    let (project, manifest) = fixture(
        r#"[publish]
exclude = ["private.env"]
"#,
    );
    assert_eq!(
        preflight_ignored_inputs(project.path(), &manifest).unwrap(),
        0
    );
}

#[cfg(unix)]
#[test]
fn tracked_clean_allowlist_admits_a_narrow_ignored_input() {
    let (project, manifest) = fixture(
        r#"[publish]
exclude = ["private.env"]
"#,
    );
    fs::write(project.path().join("generated.wasm"), "wasm\n").unwrap();
    fs::write(
        project.path().join(".gitignore"),
        "private.env\ngenerated.wasm\n",
    )
    .unwrap();
    fs::write(
        project.path().join(IGNORED_INPUT_ALLOW_FILE),
        "generated.wasm\n",
    )
    .unwrap();
    run_git(
        project.path(),
        &["add", ".gitignore", IGNORED_INPUT_ALLOW_FILE],
    );
    run_git(project.path(), &["commit", "-m", "allow generated input"]);

    assert_eq!(
        preflight_ignored_inputs(project.path(), &manifest).unwrap(),
        1
    );
}

#[cfg(unix)]
#[test]
fn ignored_input_inside_an_initialized_submodule_is_rejected() {
    let child = tempfile::tempdir().unwrap();
    run_git(child.path(), &["init"]);
    fs::write(child.path().join(".gitignore"), "private.env\n").unwrap();
    fs::write(child.path().join("lib.txt"), "runtime\n").unwrap();
    run_git(child.path(), &["add", ".gitignore", "lib.txt"]);
    run_git(child.path(), &["commit", "-m", "child"]);

    let root = tempfile::tempdir().unwrap();
    run_git(root.path(), &["init"]);
    let source = manifest_text("");
    fs::write(root.path().join(".zpkg.toml"), &source).unwrap();
    let status = Command::new("git")
        .arg("-C")
        .arg(root.path())
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            child.path().to_str().unwrap(),
            "vendor/client",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap();
    assert!(status.success());
    fs::write(
        root.path().join("vendor/client/private.env"),
        "TOKEN=submodule-secret\n",
    )
    .unwrap();
    run_git(
        root.path(),
        &["add", ".zpkg.toml", ".gitmodules", "vendor/client"],
    );
    run_git(root.path(), &["commit", "-m", "root"]);

    let manifest = harden_manifest(Manifest::parse(&source).unwrap());
    let error = preflight_ignored_inputs(root.path(), &manifest).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("vendor/client/private.env"), "{message}");
}

#[cfg(unix)]
#[test]
fn ignored_root_legal_file_in_a_polyglot_package_is_rejected() {
    let project = tempfile::tempdir().unwrap();
    run_git(project.path(), &["init"]);
    let source = manifest_text(
        r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
    );
    fs::write(project.path().join(".zpkg.toml"), &source).unwrap();
    fs::write(project.path().join(".gitignore"), "NOTICE.private\n").unwrap();
    fs::create_dir_all(project.path().join("clients/ts")).unwrap();
    fs::write(project.path().join("clients/ts/package.json"), "{}\n").unwrap();
    fs::write(project.path().join("NOTICE.private"), "TOKEN=secret\n").unwrap();
    run_git(
        project.path(),
        &["add", ".zpkg.toml", ".gitignore", "clients/ts/package.json"],
    );
    run_git(project.path(), &["commit", "-m", "polyglot fixture"]);

    let manifest = harden_manifest(Manifest::parse(&source).unwrap());
    let error = preflight_ignored_inputs(project.path(), &manifest).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("NOTICE.private"), "{message}");
}

#[cfg(unix)]
#[test]
fn dirty_allowlist_cannot_relax_the_publication_boundary() {
    let (project, manifest) = fixture(
        r#"[publish]
exclude = ["private.env"]
"#,
    );
    fs::write(
        project.path().join(IGNORED_INPUT_ALLOW_FILE),
        "generated.wasm\n",
    )
    .unwrap();
    run_git(project.path(), &["add", IGNORED_INPUT_ALLOW_FILE]);
    run_git(project.path(), &["commit", "-m", "track allowlist"]);
    fs::write(project.path().join(IGNORED_INPUT_ALLOW_FILE), "**\n").unwrap();

    let error = preflight_ignored_inputs(project.path(), &manifest).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("committed and clean"), "{message}");
}

#[test]
fn allowlist_patterns_are_project_relative_and_non_negated() {
    for invalid in [
        "",
        "*",
        "**",
        "**/*",
        "!secret",
        "/secret",
        "./secret",
        "../secret",
        "C:/secret",
        "a//b",
        "a\\b",
    ] {
        assert!(git::validate_allow_pattern(invalid).is_err(), "{invalid}");
    }
    for valid in ["generated.wasm", "dist/**/*.wasm", "clients/*/generated/**"] {
        git::validate_allow_pattern(valid).unwrap();
    }
}
