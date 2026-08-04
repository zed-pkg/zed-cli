use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

fn run(args: &[&str], home: &Path, bin_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(args)
        .env("ZED_PKG_HOME", home)
        .env("ZED_PKG_GLOBAL_BIN_DIR", bin_dir)
        .env("ZED_PKG_REGISTRY", "file:///unused")
        .env_remove("ZED_PKG_TOKEN")
        .env_remove("ZED_PKG_INTERACTIVE")
        .output()
        .expect("run zed")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn profile_root(home: &Path) -> PathBuf {
    home.join("global")
        .join("profiles")
        .join("acme")
        .join("tool")
}

fn add_profile(home: &Path, requested: &str, command: &str, bytes: &[u8]) -> PathBuf {
    let root = profile_root(home);
    let package_bin = root.join("zed_modules").join(".bin").join(command);
    fs::create_dir_all(package_bin.parent().expect("profile bin parent")).unwrap();
    fs::write(&package_bin, bytes).unwrap();
    fs::write(root.join(".zpkg.lock"), "version = 1\n").unwrap();
    fs::write(
        root.join(".zed-global-profile.json"),
        serde_json::to_vec_pretty(&json!({
            "package": "acme/tool",
            "requested": requested,
        }))
        .unwrap(),
    )
    .unwrap();
    root
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(windows)]
fn destination_name(command: &str) -> String {
    if command.to_ascii_lowercase().ends_with(".exe") {
        command.to_string()
    } else {
        format!("{command}.exe")
    }
}

#[cfg(not(windows))]
fn destination_name(command: &str) -> String {
    command.to_string()
}

fn write_managed_state(home: &Path, destination: &str, owned_bytes: &[u8]) {
    let global = home.join("global");
    fs::create_dir_all(&global).unwrap();
    let mut bins = Map::new();
    bins.insert(
        destination.to_string(),
        json!({
            "package": "acme/tool",
            "sha256": sha256(owned_bytes),
        }),
    );
    fs::write(
        global.join("managed-bins.json"),
        serde_json::to_vec_pretty(&json!({ "bins": Value::Object(bins) })).unwrap(),
    )
    .unwrap();
}

#[test]
fn root_help_composes_every_modular_command_family() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(&["--help"], home.path(), &bin_dir);

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for command in ["global", "fetch", "interop", "overtake", "develop"] {
        assert!(
            text.contains(command),
            "root help omitted {command}: {text}"
        );
    }
}

#[test]
fn help_global_lists_the_complete_lifecycle_and_path_override() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(&["help", "global"], home.path(), &bin_dir);

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for command in ["install", "uninstall", "list", "bin-dir"] {
        assert!(
            text.contains(command),
            "global help omitted {command}: {text}"
        );
    }
    assert!(text.contains("--global-bin-dir"));
}

#[test]
fn global_bin_dir_uses_environment_and_cli_overrides() {
    let home = tempfile::tempdir().expect("temporary home");
    let environment_bin = home.path().join("environment-bin");
    let output = run(&["global", "bin-dir"], home.path(), &environment_bin);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        environment_bin.display().to_string()
    );

    let cli_bin = home.path().join("cli-bin");
    let output = run(
        &[
            "--global-bin-dir",
            cli_bin.to_str().expect("UTF-8 test path"),
            "global",
            "bin-dir",
        ],
        home.path(),
        &environment_bin,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), cli_bin.display().to_string());
}

#[test]
fn empty_global_list_is_network_free_and_deterministic() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(&["global", "list"], home.path(), &bin_dir);

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("no global package profiles installed"));
    assert!(text.contains(&bin_dir.display().to_string()));
}

#[test]
fn global_list_reads_an_offline_profile_and_its_commands() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    add_profile(home.path(), "acme/tool@^1", "acme-tool", b"tool bytes");

    let output = run(&["global", "list"], home.path(), &bin_dir);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("acme/tool@unknown"), "{text}");
    assert!(text.contains("requested `acme/tool@^1`"), "{text}");
    assert!(text.contains("bins: acme-tool"), "{text}");
}

#[test]
fn install_global_alias_rejects_an_invalid_identity_before_network_io() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(
        &["install", "--global", "not-a-package-identity"],
        home.path(),
        &bin_dir,
    );

    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(
        error.contains("invalid package spec"),
        "unexpected error for invalid global package identity: {error}"
    );
}

#[test]
fn uninstall_global_alias_rejects_versioned_selectors() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(
        &["uninstall", "--global", "acme/tool@1.0.0"],
        home.path(),
        &bin_dir,
    );

    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(
        error.contains("selectors must be package identities without versions"),
        "unexpected versioned-selector error: {error}"
    );
}

#[test]
fn uninstall_removes_an_unchanged_zed_owned_command() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let profile = add_profile(home.path(), "acme/tool", "acme-tool", b"profile command");
    let destination_name = destination_name("acme-tool");
    let destination = bin_dir.join(&destination_name);
    let owned = b"installed command";
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(&destination, owned).unwrap();
    write_managed_state(home.path(), &destination_name, owned);

    let output = run(&["global", "uninstall", "acme/tool"], home.path(), &bin_dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!profile.exists());
    assert!(!destination.exists());
}

#[test]
fn uninstall_preserves_a_command_modified_after_installation() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let profile = add_profile(home.path(), "acme/tool", "acme-tool", b"profile command");
    let destination_name = destination_name("acme-tool");
    let destination = bin_dir.join(&destination_name);
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(&destination, b"user replacement").unwrap();
    write_managed_state(home.path(), &destination_name, b"original zed bytes");

    let output = run(&["global", "uninstall", "acme/tool"], home.path(), &bin_dir);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!profile.exists());
    assert_eq!(fs::read(&destination).unwrap(), b"user replacement");
    assert!(
        stderr(&output).contains("changed after Zed installed it"),
        "missing tamper-preservation warning: {}",
        stderr(&output)
    );
}
