use std::{fs, process::Command};

fn fixture(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("zed-env-contract-")
        .tempdir()
        .expect("create isolated env-contract fixture");
    let path = dir.path().join(".zpkg.toml");
    fs::write(&path, contents).expect("write env-contract fixture");
    (dir, path)
}

#[test]
fn accepts_conditional_requirements_when_not_selected() {
    let (_fixture, manifest) = fixture(
        r#"
[package]
name = "locks"

[[env.vars]]
name = "ORES_LOCK_BACKEND"
default = "postgres"
enum = ["postgres", "redis"]

[[env.vars]]
name = "REDIS_URL"
type = "url"
secret = true
required_when = "ORES_LOCK_BACKEND == 'redis'"
"#,
    );
    let status = Command::new(env!("CARGO_BIN_EXE_zed-env-contract"))
        .arg("check")
        .arg(&manifest)
        .env_remove("ORES_LOCK_BACKEND")
        .env_remove("REDIS_URL")
        .status()
        .expect("run env-contract check");
    assert!(status.success());
}

#[test]
fn requires_conditionally_selected_secret() {
    let (_fixture, manifest) = fixture(
        r#"
[[env.vars]]
name = "ORES_LOCK_BACKEND"
enum = ["postgres", "redis"]

[[env.vars]]
name = "REDIS_URL"
type = "url"
secret = true
required_when = "ORES_LOCK_BACKEND == 'redis'"
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_zed-env-contract"))
        .arg("check")
        .arg(&manifest)
        .env("ORES_LOCK_BACKEND", "redis")
        .env_remove("REDIS_URL")
        .output()
        .expect("run env-contract check");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("REDIS_URL"));
}

#[test]
fn rejects_secret_defaults() {
    let (_fixture, manifest) = fixture(
        r#"
[[env.vars]]
name = "TOKEN"
secret = true
default = "do-not-do-this"
"#,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_zed-env-contract"))
        .arg("check")
        .arg(&manifest)
        .output()
        .expect("run env-contract check");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not declare a default"));
}
