use super::*;

fn manifest(extra: &str) -> Manifest {
    Manifest::parse(&format!(
        r#"[package]
org = "acme"
name = "pack-inputs-extended"
version = "1.2.3"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/pack-inputs-extended.git"

{extra}
"#
    ))
    .unwrap()
}

#[cfg(unix)]
fn git(project: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Zed Extended Pack Inputs")
        .env(
            "GIT_AUTHOR_EMAIL",
            "zed-extended-pack-inputs@example.invalid",
        )
        .env("GIT_COMMITTER_NAME", "Zed Extended Pack Inputs")
        .env(
            "GIT_COMMITTER_EMAIL",
            "zed-extended-pack-inputs@example.invalid",
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn hardening_excludes_the_allowlist_control_file() {
    let manifest = harden_manifest(manifest(""));
    for expected in [IGNORED_INPUT_ALLOW_FILE, "**/.zedinclude"] {
        assert!(
            manifest
                .publish
                .exclude
                .iter()
                .any(|pattern| pattern == expected),
            "missing exclusion for {expected}"
        );
    }
}

#[test]
fn allowlist_patterns_are_bounded_and_project_relative() {
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
    for valid in [
        "generated.wasm",
        "dist/**/*.wasm",
        "clients/*/generated/**",
    ] {
        git::validate_allow_pattern(valid).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn tracked_clean_allowlist_admits_a_narrow_generated_input() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::write(project.path().join(".gitignore"), "generated.wasm\n").unwrap();
    fs::write(
        project.path().join(IGNORED_INPUT_ALLOW_FILE),
        "generated.wasm\n",
    )
    .unwrap();
    fs::write(project.path().join("payload.txt"), "runtime\n").unwrap();
    fs::write(project.path().join("generated.wasm"), "generated\n").unwrap();
    git(
        project.path(),
        &[
            "add",
            "--",
            ".gitignore",
            IGNORED_INPUT_ALLOW_FILE,
            "payload.txt",
        ],
    );
    git(project.path(), &["commit", "-m", "review generated input"]);

    assert_eq!(
        preflight_git_ignored(project.path(), &harden_manifest(manifest(""))).unwrap(),
        1
    );
}

#[cfg(unix)]
#[test]
fn untracked_allowlist_cannot_relax_the_publication_boundary() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::write(project.path().join(".gitignore"), "generated.wasm\n").unwrap();
    fs::write(
        project.path().join(IGNORED_INPUT_ALLOW_FILE),
        "generated.wasm\n",
    )
    .unwrap();
    fs::write(project.path().join("generated.wasm"), "generated\n").unwrap();
    git(project.path(), &["add", "--", ".gitignore"]);
    git(project.path(), &["commit", "-m", "ignore generated input"]);

    let error =
        preflight_git_ignored(project.path(), &harden_manifest(manifest(""))).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("must be tracked"), "{message}");
}

#[cfg(unix)]
#[test]
fn dirty_allowlist_cannot_relax_the_publication_boundary() {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    fs::write(project.path().join(".gitignore"), "generated.wasm\n").unwrap();
    fs::write(
        project.path().join(IGNORED_INPUT_ALLOW_FILE),
        "generated.wasm\n",
    )
    .unwrap();
    fs::write(project.path().join("generated.wasm"), "generated\n").unwrap();
    git(
        project.path(),
        &["add", "--", ".gitignore", IGNORED_INPUT_ALLOW_FILE],
    );
    git(project.path(), &["commit", "-m", "track allowlist"]);
    fs::write(project.path().join(IGNORED_INPUT_ALLOW_FILE), "**\n").unwrap();

    let error =
        preflight_git_ignored(project.path(), &harden_manifest(manifest(""))).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("committed and clean"), "{message}");
}

#[cfg(unix)]
#[test]
fn ignored_input_inside_initialized_submodule_is_rejected() {
    let child = tempfile::tempdir().unwrap();
    git(child.path(), &["init"]);
    fs::write(child.path().join(".gitignore"), "private.env\n").unwrap();
    fs::write(child.path().join("lib.txt"), "runtime\n").unwrap();
    git(child.path(), &["add", "--", ".gitignore", "lib.txt"]);
    git(child.path(), &["commit", "-m", "child baseline"]);

    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init"]);
    fs::write(root.path().join(".zpkg.toml"), "fixture\n").unwrap();
    let output = Command::new("git")
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
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "submodule add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(
        root.path().join("vendor/client/private.env"),
        "TOKEN=submodule-secret\n",
    )
    .unwrap();
    git(
        root.path(),
        &["add", "--", ".zpkg.toml", ".gitmodules", "vendor/client"],
    );
    git(root.path(), &["commit", "-m", "root baseline"]);

    let error = preflight_git_ignored(root.path(), &harden_manifest(manifest(""))).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("vendor/client/private.env"), "{message}");
}

#[cfg(unix)]
fn polyglot_legal_fixture(
    manifest_extra: &str,
    target_has_legal_file: bool,
) -> (tempfile::TempDir, Manifest) {
    let project = tempfile::tempdir().unwrap();
    git(project.path(), &["init"]);
    let manifest = manifest(manifest_extra);
    fs::write(
        project.path().join(".zpkg.toml"),
        manifest.to_toml_string().unwrap(),
    )
    .unwrap();
    fs::write(project.path().join(".gitignore"), "NOTICE.private\n").unwrap();
    fs::create_dir_all(project.path().join("clients/ts")).unwrap();
    fs::write(
        project.path().join("clients/ts/package.json"),
        "{\"name\":\"fixture\"}\n",
    )
    .unwrap();
    if target_has_legal_file {
        fs::write(
            project.path().join("clients/ts/NOTICE.private"),
            "target notice\n",
        )
        .unwrap();
    }
    fs::write(
        project.path().join("NOTICE.private"),
        "TOKEN=root-legal-secret\n",
    )
    .unwrap();
    git(
        project.path(),
        &["add", "--", ".zpkg.toml", ".gitignore", "clients/ts"],
    );
    git(project.path(), &["commit", "-m", "polyglot baseline"]);
    (project, harden_manifest(manifest))
}

#[cfg(unix)]
#[test]
fn ignored_root_legal_file_copied_into_polyglot_target_is_rejected() {
    let (project, manifest) = polyglot_legal_fixture(
        r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
        false,
    );

    let error = preflight_git_ignored(project.path(), &manifest).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("NOTICE.private"), "{message}");
    assert!(message.contains("root legal-file copy"), "{message}");
}

#[cfg(unix)]
#[test]
fn publish_exclusion_cannot_hide_an_always_included_root_legal_file() {
    let (project, manifest) = polyglot_legal_fixture(
        r#"[publish]
exclude = ["NOTICE.private"]

[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
        false,
    );

    let error = preflight_git_ignored(project.path(), &manifest).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("NOTICE.private"), "{message}");
    assert!(message.contains("root legal-file copy"), "{message}");
}

#[cfg(unix)]
#[test]
fn target_owned_legal_file_prevents_copying_the_ignored_root_file() {
    let (project, manifest) = polyglot_legal_fixture(
        r#"[targets.nodejs]
dir = "clients/ts"
adapter = "node"
"#,
        true,
    );

    assert_eq!(preflight_git_ignored(project.path(), &manifest).unwrap(), 1);
}
