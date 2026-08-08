use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use flate2::{Compression, GzBuilder};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zed_interfaces::{
    ENVIRONMENT_LOCK_SCHEMA_VERSION, EnvironmentLock, LockedArtifact, LockedArtifactFormat,
    LockedExecutable, LockedInstall, LockedPlatform, LockedSource, LockedSourceKind, LockedTool,
};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed-tool"))
}

fn target() -> &'static str {
    if cfg!(windows) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(target_os = "macos") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

fn executable_source() -> &'static str {
    if cfg!(windows) {
        "bin/hello.cmd"
    } else {
        "bin/hello"
    }
}

fn executable_body() -> &'static [u8] {
    if cfg!(windows) {
        b"@echo hello\r\n"
    } else {
        b"#!/bin/sh\nprintf 'hello\\n'\n"
    }
}

fn archive(path: &Path, source: &str, body: &[u8]) {
    let file = fs::File::create(path).unwrap();
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, format!("pkg/{source}"), body)
        .unwrap();
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap();
}

fn digest(path: &Path) -> (String, u64) {
    let bytes = fs::read(path).unwrap();
    (hex::encode(Sha256::digest(&bytes)), bytes.len() as u64)
}

fn locked_tool(name: &str, sha256: &str, size: u64, executable_name: &str) -> LockedTool {
    LockedTool {
        requirement: "1".to_string(),
        resolved: "1.0.0".to_string(),
        backend: "http".to_string(),
        backend_version: Some("1.0.0".to_string()),
        backend_options_digest_sha256: None,
        source: LockedSource {
            kind: LockedSourceKind::Http,
            locator: format!("https://example.invalid/{name}-1.0.0.tar.gz"),
            revision: None,
            tree_sha256: None,
            immutable: false,
            portable: false,
            extensions: BTreeMap::new(),
        },
        artifact: LockedArtifact {
            sha256: sha256.to_string(),
            size,
            format: LockedArtifactFormat::TarGz,
            mirrors: Vec::new(),
            signatures: Vec::new(),
            extensions: BTreeMap::new(),
        },
        platform: LockedPlatform {
            target: target().to_string(),
            os: Some(std::env::consts::OS.to_string()),
            arch: Some(std::env::consts::ARCH.to_string()),
            libc: None,
            abi: None,
        },
        install: LockedInstall {
            root: ".".to_string(),
            bin_dirs: vec!["bin".to_string()],
            executables: vec![LockedExecutable {
                name: executable_name.to_string(),
                path: executable_source().to_string(),
                aliases: vec![format!("{executable_name}-alias")],
            }],
            layout_digest_sha256: None,
            extensions: BTreeMap::new(),
        },
        extensions: BTreeMap::new(),
    }
}

fn fixture(tools: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, PathBuf, String) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let home = temp.path().join("home");
    fs::create_dir_all(project.join(".zed")).unwrap();
    fs::create_dir_all(home.join("cache")).unwrap();

    let draft = temp.path().join("artifact.tar.gz");
    archive(&draft, executable_source(), executable_body());
    let (sha256, size) = digest(&draft);
    fs::rename(&draft, home.join("cache").join(format!("{sha256}.tar.gz"))).unwrap();

    let mut lock = EnvironmentLock {
        schema_version: ENVIRONMENT_LOCK_SCHEMA_VERSION,
        plan_digest_sha256: "a".repeat(64),
        tools: BTreeMap::new(),
        extensions: BTreeMap::new(),
    };
    for (tool, executable) in tools {
        lock.tools.insert(
            (*tool).to_string(),
            vec![locked_tool(tool, &sha256, size, executable)],
        );
    }
    fs::write(
        project.join(".zed/environment.lock.toml"),
        lock.to_toml_string().unwrap(),
    )
    .unwrap();
    (temp, project, home, sha256)
}

fn run(project: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .current_dir(project)
        .args(args)
        .env("ZED_PKG_HOME", home)
        .env_remove("ZED_TOOL_LOCK")
        .env_remove("ZED_TOOL_JSON")
        .env_remove("ZED_TOOL_PORTABLE")
        .env_remove("ZED_TOOL_PLAN_DIGEST")
        .env_remove("ZED_TOOL_TARGET")
        .env_remove("ZED_TOOL_OFFLINE")
        .env_remove("ZED_TOOL_PROFILE")
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn verify_list_install_and_idempotent_replay_are_deterministic() {
    let (_temp, project, home, sha256) = fixture(&[("hello", "hello")]);

    let verify = run(
        &project,
        &home,
        &[
            "--json",
            "verify",
            "--portable",
            "--plan-digest",
            &"a".repeat(64),
        ],
    );
    assert_success(&verify);
    let verify: Value = serde_json::from_slice(&verify.stdout).unwrap();
    assert_eq!(verify["tools"], 1);
    assert_eq!(verify["validation"], "portable");

    let list = run(&project, &home, &["--json", "list", "--target", target()]);
    assert_success(&list);
    let list: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list[0]["name"], "hello");
    assert_eq!(list[0]["artifact_sha256"], sha256);

    let first = run(
        &project,
        &home,
        &["--json", "install", "--target", target(), "--offline"],
    );
    assert_success(&first);
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["action"], "installed");
    let state = project
        .join(".zed/tools/v1")
        .join(target())
        .join("profile.json");
    let state_before = fs::read(&state).unwrap();

    let second = run(
        &project,
        &home,
        &["--json", "install", "--target", target(), "--offline"],
    );
    assert_success(&second);
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["action"], "unchanged");
    assert_eq!(fs::read(&state).unwrap(), state_before);

    let bin = project.join(second["bin"].as_str().unwrap());
    if cfg!(windows) {
        assert!(bin.join("hello.cmd").is_file());
        assert!(bin.join("hello-alias.cmd").is_file());
    } else {
        let output = Command::new(bin.join("hello")).output().unwrap();
        assert_success(&output);
        assert_eq!(output.stdout, b"hello\n");
        assert!(bin.join("hello-alias").exists());
    }
}

#[test]
fn missing_and_tampered_cache_fail_before_profile_mutation() {
    let (_temp, project, home, sha256) = fixture(&[("hello", "hello")]);
    let cache = home.join("cache").join(format!("{sha256}.tar.gz"));
    let original = fs::read(&cache).unwrap();

    fs::remove_file(&cache).unwrap();
    let missing = run(
        &project,
        &home,
        &["install", "--target", target(), "--offline"],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("prefetch"));
    assert!(!project.join(".zed/tools/v1").join(target()).exists());

    fs::write(&cache, &original).unwrap();
    let mut tampered = original;
    let last = tampered.len() - 1;
    tampered[last] ^= 1;
    fs::write(&cache, tampered).unwrap();
    let mismatch = run(
        &project,
        &home,
        &["install", "--target", target(), "--offline"],
    );
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("hash mismatch"));
    assert!(!project.join(".zed/tools/v1").join(target()).exists());
}

#[test]
fn executable_collisions_and_online_mode_fail_closed() {
    let (_temp, project, home, _sha256) = fixture(&[("one", "shared"), ("two", "shared")]);
    let collision = run(
        &project,
        &home,
        &["install", "--target", target(), "--offline"],
    );
    assert!(!collision.status.success());
    assert!(String::from_utf8_lossy(&collision.stderr).contains("claimed by both"));
    assert!(!project.join(".zed/tools/v1").join(target()).exists());

    let online = run(&project, &home, &["install", "--target", target()]);
    assert!(!online.status.success());
    assert!(String::from_utf8_lossy(&online.stderr).contains("requires `--offline`"));
}

#[test]
fn wrong_plan_digest_and_target_fail_without_mutation() {
    let (_temp, project, home, _sha256) = fixture(&[("hello", "hello")]);
    let digest = run(
        &project,
        &home,
        &["verify", "--portable", "--plan-digest", &"b".repeat(64)],
    );
    assert!(!digest.status.success());
    assert!(String::from_utf8_lossy(&digest.stderr).contains("plan digest"));

    let target = run(
        &project,
        &home,
        &["install", "--target", "../host", "--offline"],
    );
    assert!(!target.status.success());
    assert!(String::from_utf8_lossy(&target.stderr).contains("unsupported characters"));
    assert!(!project.join(".zed/tools/v1").exists());
}
