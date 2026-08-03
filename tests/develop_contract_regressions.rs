use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const CLEAN_ENV: &[&str] = &[
    "IN_NIX_SHELL",
    "ZED_DEV_COMMAND",
    "ZED_DEV_ISOLATED_HOME",
    "ZED_DEV_NIX",
    "ZED_DEV_NIX_ACTIVE",
    "ZED_DEV_NO_INSTALL",
    "ZED_DEV_PRINT_ENV",
    "ZED_DEV_PROFILE",
    "ZED_DEV_PYTHON",
    "ZED_DEV_PYTHON_VENV",
    "ZED_DEV_SHELL",
    "ZED_DEV_VENV",
    "ZED_PKG_ALLOW_BUILD",
    "ZED_PKG_ALLOW_INSTALL_HOOKS",
    "ZED_PKG_ALLOW_NATIVE_DEPS",
    "ZED_PKG_NATIVE_MANAGER",
    "ZED_PKG_AUTH_URL",
    "ZED_PKG_FROZEN",
    "ZED_PKG_HOME",
    "ZED_PKG_REGISTRY",
    "ZED_PKG_SUPABASE_KEY",
    "ZED_PKG_SUPABASE_URL",
    "ZED_PKG_TOKEN",
];

fn project() -> TempDir {
    let project = tempfile::tempdir().expect("create project fixture");
    fs::write(project.path().join("package.json"), "{}\n").expect("write package.json");
    project
}

fn zed(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command.current_dir(root);
    for key in CLEAN_ENV {
        command.env_remove(key);
    }
    command.env("ZED_PKG_HOME", root.join(".zed-pkg-home"));
    command
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn print_env(root: &Path, extra: &[&str]) -> BTreeMap<String, String> {
    let mut command = zed(root);
    command.args([
        "dev",
        "--no-install",
        "--nix",
        "never",
        "--python-venv",
        "never",
    ]);
    command.args(extra).arg("--print-env");
    let output = command
        .output()
        .expect("print managed development environment");
    assert_success(&output);
    serde_json::from_slice(&output.stdout).expect("parse managed environment JSON")
}

#[test]
fn truthy_flags2env_spellings_drive_the_develop_command() {
    let project = project();
    let output = zed(project.path())
        .env("ZED_DEV_NO_INSTALL", "yes")
        .env("ZED_DEV_NIX", "never")
        .env("ZED_DEV_PYTHON_VENV", "never")
        .env("ZED_DEV_PRINT_ENV", "on")
        .arg("dev")
        .output()
        .expect("run environment-configured develop command");

    assert_success(&output);
    let managed: BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).expect("parse managed environment JSON");
    assert_eq!(managed.get("ZED_DEV").map(String::as_str), Some("1"));
}

#[test]
fn isolated_home_does_not_copy_existing_provider_credentials() {
    let project = project();
    let source_home = project.path().join("source-home");
    fs::create_dir_all(source_home.join(".codex")).expect("create Codex credential directory");
    fs::create_dir_all(source_home.join(".config/gcloud"))
        .expect("create provider credential directory");
    fs::write(
        source_home.join(".codex/credentials.json"),
        "{\"token\":\"DO_NOT_COPY_CODEX_TOKEN\"}\n",
    )
    .expect("write Codex credential fixture");
    fs::write(
        source_home.join(".config/gcloud/application_default_credentials.json"),
        "{\"token\":\"DO_NOT_COPY_CLOUD_TOKEN\"}\n",
    )
    .expect("write cloud credential fixture");

    let output = zed(project.path())
        .env("HOME", &source_home)
        .env("USERPROFILE", &source_home)
        .args([
            "dev",
            "--no-install",
            "--nix",
            "never",
            "--python-venv",
            "never",
            "--isolated-home",
            "--print-env",
        ])
        .output()
        .expect("run isolated development environment");

    assert_success(&output);
    let managed: BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).expect("parse managed environment JSON");
    let expected_home = fs::canonicalize(project.path())
        .expect("canonicalize project")
        .join(".zed/dev/home");
    let expected_home_string = expected_home.to_string_lossy().into_owned();
    assert_eq!(
        managed.get("HOME").map(String::as_str),
        Some(expected_home_string.as_str())
    );
    assert!(expected_home.is_dir());
    assert!(!expected_home.join(".codex/credentials.json").exists());
    assert!(
        !expected_home
            .join(".config/gcloud/application_default_credentials.json")
            .exists()
    );
    assert!(
        !managed.values().any(|value| {
            value.contains("DO_NOT_COPY_CODEX_TOKEN") || value.contains("DO_NOT_COPY_CLOUD_TOKEN")
        }),
        "managed environment exposed a provider credential"
    );
}

#[test]
fn project_discovery_ignores_vcs_metadata_directories() {
    let fixture = tempfile::tempdir().expect("create project-discovery fixture");
    for relative in [".git/fixture", ".hg/fixture", ".jj/fixture"] {
        let directory = fixture.path().join(relative);
        fs::create_dir_all(&directory).expect("create VCS fixture directory");
        fs::write(directory.join("package.json"), "{}\n").expect("write ignored package manifest");
    }

    let managed = print_env(fixture.path(), &[]);
    let expected_root = fs::canonicalize(fixture.path())
        .expect("canonicalize fixture root")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        managed.get("ZED_DEV_PROJECT_ROOT").map(String::as_str),
        Some(expected_root.as_str())
    );
}

#[test]
fn ai_profile_path_remains_explicitly_opt_in() {
    let project = project();
    let root = fs::canonicalize(project.path()).expect("canonicalize project");
    let ai_bin = root.join(".zed/dev/profiles/ai/bin");
    let generic_bin = root.join(".zed/dev/bin");

    let default = print_env(project.path(), &[]);
    let default_paths: Vec<PathBuf> =
        env::split_paths(OsStr::new(default.get("PATH").expect("default PATH"))).collect();
    assert!(
        !default_paths.contains(&ai_bin),
        "AI tooling must not be enabled by default"
    );
    assert_eq!(
        default.get("ZED_DEV_PROFILE").map(String::as_str),
        Some("default")
    );

    let enabled = print_env(project.path(), &["--profile", "ai"]);
    let enabled_paths: Vec<PathBuf> =
        env::split_paths(OsStr::new(enabled.get("PATH").expect("AI PATH"))).collect();
    let ai_index = enabled_paths
        .iter()
        .position(|path| path == &ai_bin)
        .expect("AI profile bin in PATH");
    let generic_index = enabled_paths
        .iter()
        .position(|path| path == &generic_bin)
        .expect("generic development bin in PATH");
    assert!(ai_index < generic_index);
    assert_eq!(
        enabled.get("ZED_DEV_PROFILE").map(String::as_str),
        Some("ai")
    );
}

#[test]
fn root_version_and_legacy_help_remain_separate_from_develop_flags() {
    let project = project();

    let version = zed(project.path())
        .arg("--version")
        .output()
        .expect("render version");
    assert_success(&version);
    assert!(combined(&version).contains("zed"));

    for arguments in [["install", "--help"], ["help", "install"]] {
        let output = zed(project.path())
            .args(arguments)
            .output()
            .expect("render legacy install help");
        assert_success(&output);
        let text = combined(&output);
        assert!(
            text.contains("install"),
            "legacy help omitted install: {text}"
        );
        for develop_only in ["--isolated-home", "--print-env", "--python-venv", "--venv"] {
            assert!(
                !text.contains(develop_only),
                "legacy help unexpectedly contains {develop_only}: {text}"
            );
        }
    }

    let root_help = zed(project.path())
        .arg("--help")
        .output()
        .expect("render root help");
    assert_success(&root_help);
    let root_text = combined(&root_help);
    assert!(root_text.contains("develop"));
    assert!(root_text.contains("dev"));
    assert!(root_text.contains("virtual development"));
}
