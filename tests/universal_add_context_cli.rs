//! Universal add context is CLI-visible: parse project and target without
//! contacting zpkg.net, invoking native managers or mutating source manifests.
use clap::Parser;
use std::path::PathBuf;
use zed_cli::cli::{Cli, Cmd};

#[test]
fn explicit_project_and_target_are_accepted_for_add() {
    let cli = Cli::try_parse_from([
        "zed",
        "add",
        "acme/answer@^1",
        "--project",
        "apps/api",
        "--target",
        "attolang",
    ])
    .expect("parse universal add selectors");
    match cli.cmd {
        Cmd::Add {
            spec,
            project,
            target,
        } => {
            assert_eq!(spec, "acme/answer@^1");
            assert_eq!(project, Some(PathBuf::from("apps/api")));
            assert_eq!(target.as_deref(), Some("attolang"));
        }
        _ => panic!("expected add command"),
    }
}

#[test]
fn normal_add_remains_backwards_compatible() {
    let cli =
        Cli::try_parse_from(["zed", "add", "acme/answer@^1"]).expect("parse legacy add syntax");
    match cli.cmd {
        Cmd::Add { spec, .. } => assert_eq!(spec, "acme/answer@^1"),
        _ => panic!("expected add command"),
    }
}

#[test]
fn global_options_work_with_explicit_add_selectors() {
    let cli = Cli::try_parse_from([
        "zed",
        "--registry",
        "file:///tmp/fixture",
        "add",
        "acme/math",
        "--target",
        "rust",
        "--project",
        "workspace/crate",
    ])
    .expect("parse global+scoped add");
    match cli.cmd {
        Cmd::Add {
            project, target, ..
        } => {
            assert_eq!(project, Some(PathBuf::from("workspace/crate")));
            assert_eq!(target.as_deref(), Some("rust"));
        }
        _ => panic!("expected add command"),
    }
}

#[test]
fn project_and_target_support_equals_form_and_reverse_order() {
    for args in [
        vec![
            "zed",
            "add",
            "acme/answer@^1",
            "--project=apps/api",
            "--target=attolang",
        ],
        vec![
            "zed",
            "add",
            "--target=attolang",
            "acme/answer@^1",
            "--project=apps/api",
        ],
    ] {
        let cli = Cli::try_parse_from(args).expect("parse equivalent add selector spellings");
        match cli.cmd {
            Cmd::Add {
                project, target, ..
            } => {
                assert_eq!(project, Some(PathBuf::from("apps/api")));
                assert_eq!(target.as_deref(), Some("attolang"));
            }
            _ => panic!("expected scoped add command"),
        }
    }
}

// Execute the actual binary with isolated per-process environments. This is
// the only reliable way to prove Clap's env handling and the embedded
// flags-2-env admission contract without mutating process-global env vars.
fn consumer(path: &std::path::Path, target: &str) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(
        path.join(".zpkg.toml"),
        format!(
            r#"[package]
org = 'acme'
name = 'fixture'
version = '1.0.0'

[package.repository]
vcs = 'git'
url = 'https://example.invalid/fixture'

[dependencies]
'acme/pinned' = '^1.0'

[install]
target = '{target}'
"#
        ),
    )
    .unwrap();
}

fn zed(argv: &[&str], cwd: &std::path::Path, env: &[(&str, &str)]) -> std::process::Output {
    let home = cwd.join(".isolated-zed-home");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_zed"));
    command
        .current_dir(cwd)
        .args(argv)
        .env("ZED_PKG_HOME", home)
        .env_remove("ZED_PKG_ADD_PROJECT")
        .env_remove("ZED_PKG_TARGET")
        .env("ZED_PKG_REGISTRY", "file:///does-not-exist/zed-registry");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("start real Zed CLI")
}

fn assert_target_conflict(output: std::process::Output, expected: &str) {
    assert!(!output.status.success(), "should refuse target mutation");
    let message = String::from_utf8_lossy(&output.stderr);
    assert!(
        message.contains("cannot override configured target")
            || message.contains("cannot change Zed default target"),
        "unexpected CLI failure: {message}"
    );
    assert!(
        message.contains(expected),
        "expected target {expected}: {message}"
    );
}

#[test]
fn actual_cli_walks_up_to_nearest_ancestor_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path();
    consumer(parent, "attolang");
    let app = parent.join("apps/web");
    consumer(&app, "rust");
    let deep = app.join("src/deep");
    std::fs::create_dir_all(&deep).unwrap();

    // A nearer Zed consumer must win over the outer project, and the flag
    // reaches the real add context before any registry request.
    let result = zed(&["add", "acme/answer@^1", "--target=nodejs"], &deep, &[]);
    assert_target_conflict(result, "rust");
}

#[test]
fn actual_cli_flags_override_flags2env_environment_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path();
    let default_project = cwd.join("default");
    let selected_project = cwd.join("selected");
    consumer(&default_project, "node");
    consumer(&selected_project, "attolang");

    let requested = selected_project.to_string_lossy().into_owned();
    let supplied = default_project.to_string_lossy().into_owned();
    let result = zed(
        &[
            "add",
            "acme/answer@^1",
            "--project",
            &requested,
            "--target=rust",
        ],
        cwd,
        &[
            ("ZED_PKG_ADD_PROJECT", &supplied),
            ("ZED_PKG_TARGET", "node"),
        ],
    );
    assert_target_conflict(result, "attolang");
}

#[test]
fn actual_cli_env_only_selectors_reach_flags2env_and_clap() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path();
    let project = cwd.join("nested/package");
    consumer(&project, "attolang");
    let project_string = project.to_string_lossy().into_owned();

    let result = zed(
        &["add", "acme/answer@^1"],
        cwd,
        &[
            ("ZED_PKG_ADD_PROJECT", &project_string),
            ("ZED_PKG_TARGET", "rust"),
        ],
    );
    assert_target_conflict(result, "attolang");
}

#[cfg(unix)]
#[test]
fn actual_cli_rejects_symlinked_zed_manifest_before_registry_or_write() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path();
    consumer(parent, "attolang");
    let child = parent.join("apps/foreign");
    std::fs::create_dir_all(&child).unwrap();
    symlink(parent.join(".zpkg.toml"), child.join(".zpkg.toml")).unwrap();

    let output = zed(&["add", "acme/http@^1"], &child, &[]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsafe project marker"), "{stderr}");
    assert!(!child.join(".zpkg.lock").exists());
    assert!(!child.join("zed_modules").exists());
}

#[cfg(unix)]
#[test]
fn explicit_project_cannot_use_symlinked_zed_manifest() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path();
    consumer(parent, "rust");
    let child = parent.join("apps/foreign");
    std::fs::create_dir_all(&child).unwrap();
    symlink(parent.join(".zpkg.toml"), child.join(".zpkg.toml")).unwrap();

    let output = zed(
        &[
            "add",
            "acme/http@^1",
            "--project=apps/foreign",
            "--target=rust",
        ],
        parent,
        &[],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsafe project marker"), "{stderr}");
    assert!(!child.join("zed_modules").exists());
    assert!(!child.join(".zpkg.lock").exists());
}
