use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn zed() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn run(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(zed());
    command.current_dir(root).args(args);
    for key in [
        "ZED_PKG_HOME",
        "ZED_PKG_TOKEN",
        "ZED_PKG_INTERACTIVE",
        "ZED_PKG_VALIDATE_MANIFEST",
        "ZED_PKG_VALIDATE_LOCK",
        "ZED_PKG_VALIDATE_REQUIRE_LOCK",
        "ZED_PKG_VALIDATE_JSON",
    ] {
        command.env_remove(key);
    }
    command
        .env("ZED_PKG_HOME", root.join("zed-home-must-not-be-created"))
        .output()
        .expect("run zed validate")
}

fn write_project(manifest: &str, lock: Option<&str>) -> tempfile::TempDir {
    let project = tempfile::tempdir().expect("temp project");
    fs::write(project.path().join(".zpkg.toml"), manifest).expect("write manifest");
    if let Some(lock) = lock {
        fs::write(project.path().join(".zpkg.lock"), lock).expect("write lock");
    }
    project
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

const TEMPLATES: &str = r#"
[package]
org = "oresoftware"
name = "ores-formal-methods-templates"
version = "0.1.0"
description = "Polyglot formal-methods templates"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/ORESoftware/ores-formal-methods-templates"

[install]
adapter = "none"
dir = ".vendor/.zed"

[tool-dependencies]
"oresoftware/typespec-json-schema-validator" = "^0.1.1"
"#;

const TEMPLATES_LOCK: &str = r#"
version = 1

[[tool]]
org = "oresoftware"
name = "typespec-json-schema-validator"
version = "0.1.1"
sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
size = 42
format = "tar.gz"
vcs_tag = "v0.1.1"
vcs_commit = "66aff520ab946f7d9b116e8c4f39423fa70e9558"
source = "https://registry.zpkg.net"
"#;

const FORMAL_METHODS_RS: &str = r#"
[package]
org = "oresoftware"
name = "formal-methods-rs"
version = "0.1.0"
description = "Formal-methods runner and polyglot SDKs"
license = "Apache-2.0"

[package.repository]
vcs = "git"
url = "https://github.com/ORESoftware/formal-methods.rs"

[install]
adapter = "none"
dir = ".vendor/.zed"
"#;

const ORES_WIT: &str = r#"
[package]
org = "oresoftware"
name = "ores-wit"
version = "0.1.0"
description = "WIT contract validation and binding orchestration"
license = "MIT"
language = "rust"

[package.repository]
vcs = "git"
url = "https://github.com/ORESoftware/ores-wit"

[build]
command = "cargo build --release --locked --bin ores-wit"
outputs = ["target/release/ores-wit"]
outputs_windows = ["target/release/ores-wit.exe"]

[bin]
ores-wit = "target/release/ores-wit"

[install]
adapter = "none"
dir = ".vendor/.zed"
"#;

#[test]
fn templates_accept_a_frozen_tool_pin_without_materializing_a_runtime_dependency() {
    let project = write_project(TEMPLATES, Some(TEMPLATES_LOCK));
    let output = run(project.path(), &["validate", "--require-lock", "--json"]);
    assert_success(&output);

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("validation report json");
    assert_eq!(report["manifest"]["direct_requirements"], 0);
    assert_eq!(report["direct_requirements_checked"], 0);
    assert_eq!(report["warnings"], serde_json::json!([]));
}

#[test]
fn tandem_repositories_are_independent_zero_zed_dependency_packages() {
    for manifest in [FORMAL_METHODS_RS, ORES_WIT] {
        let project = write_project(manifest, None);
        let output = run(project.path(), &["validate", "--json"]);
        assert_success(&output);

        let report: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("validation report json");
        assert_eq!(report["manifest"]["direct_requirements"], 0);
        assert_eq!(report["direct_requirements_checked"], 0);
        assert_eq!(report["warnings"], serde_json::json!([]));
    }
}
