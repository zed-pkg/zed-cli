use std::{fs, process::Command, time::{SystemTime, UNIX_EPOCH}};

fn fixture(contents: &str) -> std::path::PathBuf {
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("zed-env-contract-{suffix}"));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".zpkg.toml");
    fs::write(&path, contents).unwrap();
    path
}

#[test]
fn accepts_conditional_requirements_when_not_selected() {
    let manifest = fixture(r#"
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
"#);
    let status = Command::new(env!("CARGO_BIN_EXE_zed-env-contract"))
        .arg("check").arg(&manifest)
        .env_remove("ORES_LOCK_BACKEND").env_remove("REDIS_URL")
        .status().unwrap();
    assert!(status.success());
}

#[test]
fn requires_conditionally_selected_secret() {
    let manifest = fixture(r#"
[[env.vars]]
name = "ORES_LOCK_BACKEND"
enum = ["postgres", "redis"]

[[env.vars]]
name = "REDIS_URL"
type = "url"
secret = true
required_when = "ORES_LOCK_BACKEND == 'redis'"
"#);
    let output = Command::new(env!("CARGO_BIN_EXE_zed-env-contract"))
        .arg("check").arg(&manifest)
        .env("ORES_LOCK_BACKEND", "redis").env_remove("REDIS_URL")
        .output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("REDIS_URL"));
}

#[test]
fn rejects_secret_defaults() {
    let manifest = fixture(r#"
[[env.vars]]
name = "TOKEN"
secret = true
default = "do-not-do-this"
"#);
    let output = Command::new(env!("CARGO_BIN_EXE_zed-env-contract"))
        .arg("check").arg(&manifest).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not declare a default"));
}
