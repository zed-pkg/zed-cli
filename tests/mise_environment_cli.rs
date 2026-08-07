use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn sha256(digit: char) -> String {
    format!("sha256:{}", digit.to_string().repeat(64))
}

fn write_locked_project(root: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("mise.toml"),
        r#"
[settings]
lockfile = true

[tools]
node = "22"
"#,
    )
    .unwrap();
    fs::write(
        root.join("mise.lock"),
        format!(
            r#"
[[tools.node]]
version = "22.4.0"
backend = "core:node"

[tools.node.platforms.linux-x64]
checksum = "{}"
size = 123
url = "https://example.invalid/node.tar.xz"
"#,
            sha256('a')
        ),
    )
    .unwrap();
}

fn run_zed(project: &Path, home: &Path, args: &[&str]) -> Output {
    let empty_path = home.join("empty-path");
    fs::create_dir_all(&empty_path).unwrap();

    Command::new(zed_bin())
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ZED_PKG_HOME", home.join(".zed-pkg"))
        .env(
            "MISE_GLOBAL_CONFIG_FILE",
            home.join(".config/mise/config.toml"),
        )
        // The adapter is a parser, not a shell-out wrapper. An empty PATH proves
        // the command does not require a `mise` executable to verify a plan.
        .env("PATH", empty_path)
        .env_remove("ZED_PKG_ENV_CONFIG")
        .env_remove("ZED_PKG_ENV_LOCK")
        .env_remove("ZED_PKG_ENV_JSON")
        .env_remove("ZED_PKG_FROZEN")
        .output()
        .unwrap()
}

#[test]
fn frozen_verify_is_read_only_and_does_not_load_parent_or_global_mise_config() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    let project = workspace.join("project");

    fs::create_dir_all(home.join(".config/mise")).unwrap();
    fs::write(
        home.join(".config/mise/config.toml"),
        "[tools]\npython = \"latest\"\n",
    )
    .unwrap();
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("mise.toml"), "[tools]\nruby = \"latest\"\n").unwrap();
    write_locked_project(&project);

    let config_before = fs::read(project.join("mise.toml")).unwrap();
    let lock_before = fs::read(project.join("mise.lock")).unwrap();
    let output = run_zed(
        &project,
        &home,
        &[
            "env",
            "verify",
            "mise",
            "--config",
            "mise.toml",
            "--lock",
            "mise.lock",
            "--frozen",
            "--json",
        ],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["manager"], "mise");
    assert_eq!(result["verified"], true);
    assert_eq!(result["tools"], 1);
    assert_eq!(result["config"], "mise.toml");
    assert_eq!(result["lock"], "mise.lock");
    assert_eq!(
        result["environment_plan_sha256"].as_str().unwrap().len(),
        64
    );

    assert_eq!(fs::read(project.join("mise.toml")).unwrap(), config_before);
    assert_eq!(fs::read(project.join("mise.lock")).unwrap(), lock_before);
    let mut project_entries = fs::read_dir(&project)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    project_entries.sort();
    assert_eq!(
        project_entries,
        vec![OsString::from("mise.lock"), OsString::from("mise.toml"),]
    );
}

#[test]
fn import_json_exposes_only_the_project_local_normalized_plan() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    write_locked_project(&project);

    let output = run_zed(
        &project,
        &home,
        &[
            "env",
            "import",
            "mise",
            "--config",
            "mise.toml",
            "--lock",
            "mise.lock",
            "--frozen",
            "--json",
        ],
    );

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["schema"], 1);
    assert_eq!(plan["activation"], "frozen-install");
    assert_eq!(plan["tools"]["node"]["requirement"], "22");
    assert_eq!(plan["tools"]["node"]["resolved"], "22.4.0");
    assert_eq!(plan["tools"]["node"]["provider"], "core");
    assert_eq!(plan["tools"]["node"]["backend"], "core:node");
    assert!(plan["tools"].get("python").is_none());
    assert!(plan["tools"].get("ruby").is_none());
}

#[test]
fn dot_mise_toml_discovers_the_shared_mise_lock_name() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join(".mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
    fs::write(
        project.join("mise.lock"),
        format!(
            "[[tools.node]]\nversion = \"22.4.0\"\nbackend = \"core:node\"\n[tools.node.platforms.linux-x64]\nchecksum = \"{}\"\n",
            sha256('c')
        ),
    )
    .unwrap();

    let output = run_zed(
        &project,
        &home,
        &["env", "verify", "mise", "--frozen", "--json"],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["config"], ".mise.toml");
    assert_eq!(result["lock"], "mise.lock");
    assert!(!project.join(".mise.lock").exists());
}

#[test]
fn frozen_verify_fails_closed_on_ambiguous_or_incomplete_project_state() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("mise.toml"), "[tools]\nnode = \"22\"\n").unwrap();
    fs::write(project.join(".mise.toml"), "[tools]\npython = \"3.12\"\n").unwrap();

    let ambiguous = run_zed(
        &project,
        &home,
        &["env", "verify", "mise", "--frozen", "--json"],
    );
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stderr).contains("multiple project-local"));

    fs::remove_file(project.join(".mise.toml")).unwrap();
    let unlocked = run_zed(
        &project,
        &home,
        &[
            "env",
            "verify",
            "mise",
            "--config",
            "mise.toml",
            "--frozen",
            "--json",
        ],
    );
    assert!(!unlocked.status.success());
    assert!(
        String::from_utf8_lossy(&unlocked.stderr).contains("requires a project-local lockfile")
    );
}

#[test]
fn frozen_verify_reports_platform_backend_and_requirement_drift() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("mise.toml"),
        "[settings]\nlockfile_platforms = [\"linux-x64\", \"macos-arm64\"]\n[tools]\n\"aqua:jqlang/jq\" = \"^1.7\"\n",
    )
    .unwrap();

    let write_lock = |backend: &str, version: &str, include_macos: bool| {
        let macos = if include_macos {
            format!(
                "[tools.\"aqua:jqlang/jq\".platforms.macos-arm64]\nchecksum = \"{}\"\n",
                sha256('b')
            )
        } else {
            String::new()
        };
        fs::write(
            project.join("mise.lock"),
            format!(
                "[[tools.\"aqua:jqlang/jq\"]]\nversion = \"{version}\"\nbackend = \"{backend}\"\n[tools.\"aqua:jqlang/jq\".platforms.linux-x64]\nchecksum = \"{}\"\n{macos}",
                sha256('a')
            ),
        )
        .unwrap();
    };

    write_lock("aqua:jqlang/jq", "1.7.1", false);
    let missing_platform = run_zed(
        &project,
        &home,
        &["env", "verify", "mise", "--frozen", "--json"],
    );
    assert!(!missing_platform.status.success());
    assert!(
        String::from_utf8_lossy(&missing_platform.stderr)
            .contains("missing requested locked platform")
    );

    write_lock("core:node", "1.7.1", true);
    let backend_drift = run_zed(
        &project,
        &home,
        &["env", "verify", "mise", "--frozen", "--json"],
    );
    assert!(!backend_drift.status.success());
    assert!(String::from_utf8_lossy(&backend_drift.stderr).contains("mise backend drift"));

    write_lock("aqua:jqlang/jq", "1.6.0", true);
    let version_drift = run_zed(
        &project,
        &home,
        &["env", "verify", "mise", "--frozen", "--json"],
    );
    assert!(!version_drift.status.success());
    assert!(String::from_utf8_lossy(&version_drift.stderr).contains("mise version drift"));

    write_lock("aqua:jqlang/jq", "1.7.1", true);
    let valid = run_zed(
        &project,
        &home,
        &["env", "verify", "mise", "--frozen", "--json"],
    );
    assert!(
        valid.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&valid.stderr)
    );
}
