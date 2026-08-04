use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn write_plan(project: &Path) {
    fs::create_dir_all(project).unwrap();
    fs::write(
        project.join("zed-env.json"),
        r#"{
  "schema": 2,
  "tools": {
    "node": [
      {"requirement": "22.4.0"},
      {"requirement": "20.15.1"}
    ]
  },
  "env": {"APP_ENV": "test", "RETRIES": 3},
  "vars": {"release": {"channel": "stable"}},
  "tasks": {
    "prepare": {"run": ["echo prepare"]},
    "setup": {
      "description": "Restore dependencies",
      "aliases": ["bootstrap"],
      "depends": ["prepare"],
      "run": ["zed install --frozen", "cargo check"]
    }
  },
  "platforms": ["linux-x64", "macos-arm64"],
  "activation": "none"
}
"#,
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
        .env("PATH", empty_path)
        .env_remove("ZED_PKG_ENV_PLAN")
        .env_remove("ZED_PKG_ENV_OUTPUT")
        .env_remove("ZED_PKG_ENV_JSON")
        .env_remove("ZED_PKG_ENV_CHECK")
        .env_remove("ZED_PKG_ENV_WRITE")
        .env_remove("ZED_PKG_UPDATE_CHECK")
        .output()
        .unwrap()
}

#[test]
fn print_write_check_and_noop_are_deterministic_and_do_not_require_mise() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    write_plan(&project);

    let print = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--output",
            ".mise.toml",
        ],
    );
    assert!(
        print.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&print.stderr)
    );
    assert!(!project.join(".mise.toml").exists());
    assert!(!project.join(".zed/mise-export-state.json").exists());
    let printed = String::from_utf8(print.stdout).unwrap();
    let value: toml::Value = toml::from_str(&printed).unwrap();
    let versions = value["tools"]["node"].as_array().unwrap();
    assert_eq!(versions[0].as_str(), Some("22.4.0"));
    assert_eq!(versions[1].as_str(), Some("20.15.1"));

    let write = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--output",
            ".mise.toml",
            "--write",
            "--json",
        ],
    );
    assert!(
        write.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&write.stderr)
    );
    let written: Value = serde_json::from_slice(&write.stdout).unwrap();
    assert_eq!(written["manager"], "mise");
    assert_eq!(written["action"], "written");
    assert_eq!(written["plan"], "zed-env.json");
    assert_eq!(written["output"], ".mise.toml");
    assert_eq!(written["plan_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(written["output_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(fs::read_to_string(project.join(".mise.toml")).unwrap(), printed);
    assert!(project.join(".zed/mise-export-state.json").is_file());

    let check = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--output",
            ".mise.toml",
            "--check",
            "--json",
        ],
    );
    assert!(
        check.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&check.stdout).unwrap()["action"],
        "verified"
    );

    let noop = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--output",
            ".mise.toml",
            "--write",
            "--json",
        ],
    );
    assert!(noop.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&noop.stdout).unwrap()["action"],
        "unchanged"
    );
}

#[test]
fn write_refuses_unowned_or_edited_manager_files() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    write_plan(&project);
    fs::write(project.join(".mise.toml"), "[tools]\nnode = \"18\"\n").unwrap();

    let unowned = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--write",
        ],
    );
    assert!(!unowned.status.success());
    assert!(String::from_utf8_lossy(&unowned.stderr).contains("hand-authored"));
    assert_eq!(
        fs::read_to_string(project.join(".mise.toml")).unwrap(),
        "[tools]\nnode = \"18\"\n"
    );

    fs::remove_file(project.join(".mise.toml")).unwrap();
    let owned = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--write",
        ],
    );
    assert!(owned.status.success());
    fs::write(project.join(".mise.toml"), "# user edit\n").unwrap();
    let edited = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--write",
        ],
    );
    assert!(!edited.status.success());
    assert!(String::from_utf8_lossy(&edited.stderr).contains("edited"));
    assert_eq!(
        fs::read_to_string(project.join(".mise.toml")).unwrap(),
        "# user edit\n"
    );
}

#[test]
fn check_is_read_only_and_reports_drift_without_leaking_contents() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    write_plan(&project);
    fs::write(project.join(".mise.toml"), "[tools]\nnode = \"18\"\n").unwrap();
    let before = fs::read(project.join(".mise.toml")).unwrap();

    let output = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--check",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mise export drift"));
    assert!(stderr.contains("sha256"));
    assert!(!stderr.contains("node = \"18\""));
    assert_eq!(fs::read(project.join(".mise.toml")).unwrap(), before);
    assert!(!project.join(".zed/mise-export-state.json").exists());
}

#[test]
fn clap_rejects_ambiguous_write_modes_and_project_escape() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    write_plan(&project);

    let ambiguous = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--check",
            "--write",
        ],
    );
    assert!(!ambiguous.status.success());

    let escape = run_zed(
        &project,
        &home,
        &[
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.json",
            "--output",
            "../mise.toml",
            "--write",
        ],
    );
    assert!(!escape.status.success());
    assert!(String::from_utf8_lossy(&escape.stderr).contains("project-relative"));
}