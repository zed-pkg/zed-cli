use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed-env-export"))
}

fn write_plan(root: &Path, platform: &str) {
    fs::create_dir_all(root.join(".zed")).unwrap();
    let plan = serde_json::json!({
        "schema": 1,
        "tools": {
            "node": {
                "requirement": "^22",
                "resolved": "22.11.0",
                "provider": "nixpkgs",
                "backend": "nodejs_22",
                "checksums": [
                    {"algorithm": "sha256", "value": "a".repeat(64)}
                ],
                "platforms": []
            }
        },
        "system-packages": {
            "git": {
                "requirement": "2.47.0",
                "resolved": "2.47.0",
                "provider": "nixpkgs",
                "package_ref": "gitFull",
                "checksums": [
                    {"algorithm": "sha256", "value": "b".repeat(64)}
                ],
                "platforms": [platform]
            }
        },
        "platforms": [platform],
        "activation": "frozen-install",
        "sources": []
    });
    fs::write(
        root.join(".zed/environment-plan.json"),
        serde_json::to_vec_pretty(&plan).unwrap(),
    )
    .unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .current_dir(root)
        .env("PATH", "")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn devbox_export_is_real_read_only_and_idempotent() {
    let temp = tempfile::tempdir().unwrap();
    write_plan(temp.path(), "x86_64-linux");
    let plan_before = fs::read(temp.path().join(".zed/environment-plan.json")).unwrap();

    let first = run(temp.path(), &["devbox", "--json"]);
    assert!(
        first.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(result["manager"], "devbox");
    assert_eq!(result["changed"], true);
    let output_before = fs::read(temp.path().join("devbox.json")).unwrap();
    let receipt_before =
        fs::read(temp.path().join(".zed/environment-exports/devbox.json")).unwrap();
    let config: serde_json::Value = serde_json::from_slice(&output_before).unwrap();
    assert_eq!(config["packages"]["nodejs_22"]["version"], "22.11.0");
    assert_eq!(
        config["shell"]["init_hook"],
        serde_json::json!(["zed install --frozen"])
    );

    let second = run(temp.path(), &["devbox", "--json"]);
    assert!(second.status.success());
    let result: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(result["changed"], false);
    assert_eq!(
        fs::read(temp.path().join("devbox.json")).unwrap(),
        output_before
    );
    assert_eq!(
        fs::read(temp.path().join(".zed/environment-exports/devbox.json")).unwrap(),
        receipt_before
    );
    assert_eq!(
        fs::read(temp.path().join(".zed/environment-plan.json")).unwrap(),
        plan_before
    );
}

#[test]
fn flox_export_emits_manifest_hook_and_receipt() {
    let temp = tempfile::tempdir().unwrap();
    write_plan(temp.path(), "aarch64-darwin");

    let output = run(temp.path(), &["flox", "--json"]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["manager"], "flox");
    let manifest = fs::read_to_string(temp.path().join(".flox/env/manifest.toml")).unwrap();
    let value: toml::Value = toml::from_str(&manifest).unwrap();
    assert_eq!(value["version"].as_integer(), Some(1));
    assert_eq!(
        value["install"]["node"]["pkg-path"].as_str(),
        Some("nodejs_22")
    );
    assert_eq!(
        value["hook"]["on-activate"].as_str(),
        Some("zed install --frozen")
    );
    assert!(
        temp.path()
            .join(".zed/environment-exports/flox.json")
            .is_file()
    );
}

#[test]
fn existing_human_configuration_is_not_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    write_plan(temp.path(), "x86_64-linux");
    fs::write(temp.path().join("devbox.json"), b"{\"human\": true}\n").unwrap();

    let output = run(temp.path(), &["devbox"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("refusing to overwrite conflicting existing manager output")
    );
    assert_eq!(
        fs::read(temp.path().join("devbox.json")).unwrap(),
        b"{\"human\": true}\n"
    );
    assert!(
        !temp
            .path()
            .join(".zed/environment-exports/devbox.json")
            .exists()
    );
}

#[test]
fn unsupported_platform_fails_without_partial_output() {
    let temp = tempfile::tempdir().unwrap();
    write_plan(temp.path(), "x86_64-windows");

    let output = run(temp.path(), &["flox"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("flox export cannot represent platform `x86_64-windows`")
    );
    assert!(!temp.path().join(".flox/env/manifest.toml").exists());
    assert!(
        !temp
            .path()
            .join(".zed/environment-exports/flox.json")
            .exists()
    );
}

#[test]
fn output_and_receipt_paths_cannot_escape_project() {
    let temp = tempfile::tempdir().unwrap();
    write_plan(temp.path(), "x86_64-linux");

    let output = run(
        temp.path(),
        &[
            "devbox",
            "--out",
            "../devbox.json",
            "--receipt",
            "../receipt.json",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("manager output path must be normalized and project-relative")
    );
}

#[test]
fn custom_paths_are_recorded_without_absolute_machine_state() {
    let temp = tempfile::tempdir().unwrap();
    write_plan(temp.path(), "x86_64-linux");

    let output = run(
        temp.path(),
        &[
            "devbox",
            "--out",
            "generated/devbox.json",
            "--receipt",
            "generated/devbox.receipt.json",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["output_path"], "generated/devbox.json");
    assert_eq!(result["receipt_path"], "generated/devbox.receipt.json");
    let receipt = fs::read_to_string(temp.path().join("generated/devbox.receipt.json")).unwrap();
    assert!(!receipt.contains(temp.path().to_string_lossy().as_ref()));
}
