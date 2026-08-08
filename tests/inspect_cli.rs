use std::fs;
use std::process::Command;

use serde_json::Value;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};

const MANIFEST: &str = r#"
[package]
org = "acme"
name = "inspect-fixture"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/acme/inspect-fixture"
"#;

#[test]
fn inspect_emits_one_json_document_without_auth_or_recovery_side_effects() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    fs::write(project.path().join(MANIFEST_FILE), MANIFEST).unwrap();
    fs::write(project.path().join(LOCKFILE_FILE), "version = 1\n").unwrap();
    fs::write(
        home.path().join("credentials.toml"),
        "malformed = [credential",
    )
    .unwrap();

    let staging = project.path().join(zed_cli::transaction::STAGING_DIR);
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("sentinel"), "must-survive").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .arg("--token")
        .arg("fake-token-that-inspect-must-ignore")
        .arg("inspect")
        .arg("--format")
        .arg("json")
        .arg("--root")
        .arg(project.path().canonicalize().unwrap())
        .env("ZED_PKG_HOME", home.path())
        .env("ZED_PKG_TOKEN", "fake-env-token-that-must-not-escape")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );

    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], "1.0");
    assert_eq!(report["cli"]["implementation"], "zed-pkg");
    assert_eq!(report["cli"]["offline"], true);
    assert_eq!(report["cli"]["mutates_project"], false);
    assert_eq!(report["cli"]["loads_credentials"], false);
    assert_eq!(report["summary"]["recovery_pending"], true);
    assert_eq!(report["summary"]["frozen_ready"], false);
    assert!(
        report["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["code"] == "RECOVERY_PENDING")
    );

    let rendered = String::from_utf8(output.stdout).unwrap();
    assert!(!rendered.contains("fake-token-that-inspect-must-ignore"));
    assert!(!rendered.contains("fake-env-token-that-must-not-escape"));
    assert!(!project.path().join(MODULES_DIR).exists());
    assert_eq!(
        fs::read_to_string(staging.join("sentinel")).unwrap(),
        "must-survive"
    );
    assert_eq!(
        fs::read_to_string(project.path().join(LOCKFILE_FILE)).unwrap(),
        "version = 1\n"
    );
}

#[test]
fn inspect_schema_fixture_keeps_v1_required_surface_explicit() {
    let schema: Value =
        serde_json::from_str(include_str!("../schemas/inspect-v1.schema.json")).unwrap();
    assert_eq!(schema["title"], "Zed package inspection report v1");
    assert_eq!(
        schema["properties"]["cli"]["properties"]["offline"]["const"],
        true
    );
    assert_eq!(
        schema["properties"]["cli"]["properties"]["loads_credentials"]["const"],
        false
    );
    let required = schema["required"].as_array().unwrap();
    for key in [
        "schema_version",
        "root",
        "cli",
        "package",
        "summary",
        "diagnostics",
    ] {
        assert!(
            required.iter().any(|value| value == key),
            "missing required key {key}"
        );
    }
}
