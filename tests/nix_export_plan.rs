use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use walkdir::WalkDir;

fn zed() -> &'static str {
    env!("CARGO_BIN_EXE_zed")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn write_project(root: &Path, manifest: &str, lock: Option<&str>, files: &[(&str, &[u8])]) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join(".zpkg.toml"), manifest).unwrap();
    if let Some(lock) = lock {
        fs::write(root.join(".zpkg.lock"), lock).unwrap();
    }
    for (relative, bytes) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

fn run_plan(project: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(zed());
    command
        .current_dir(project)
        .env_clear()
        .arg("interop")
        .arg("nix")
        .arg("plan")
        .arg("export");
    for arg in args {
        command.arg(arg);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            result.insert(relative, sha256(&fs::read(entry.path()).unwrap()));
        }
    }
    result
}

fn data_manifest(extra: &str) -> String {
    format!(
        r#"[package]
org = "acme"
name = "dataset"
version = "1.2.3"
description = "immutable data"
license = "MIT"

[package.repository]
url = "https://github.com/acme/dataset"

[publish.nix]
attribute = "dataset"
systems = ["x86_64-linux", "aarch64-linux"]
outputs = ["out"]

{extra}
"#
    )
}

#[test]
fn single_data_plan_is_path_independent_sorted_and_read_only() {
    let first_root = TempDir::new().unwrap();
    let second_root = TempDir::new().unwrap();
    let first = first_root.path().join("different/absolute/path/one");
    let second = second_root.path().join("other/location/two");
    let manifest = data_manifest("");
    let files = [("data/value.txt", b"same immutable payload\n".as_slice())];
    write_project(&first, &manifest, Some("version = 1\n"), &files);
    write_project(&second, &manifest, Some("version = 1\n"), &files);
    let before_first = snapshot(&first);
    let before_second = snapshot(&second);

    let first_output = run_plan(&first, &["--frozen", "--json"], &[]);
    let second_output = run_plan(&second, &["--frozen", "--json"], &[]);
    assert!(first_output.status.success(), "{}", stderr(&first_output));
    assert!(second_output.status.success(), "{}", stderr(&second_output));
    assert_eq!(stdout(&first_output), stdout(&second_output));
    assert_eq!(snapshot(&first), before_first);
    assert_eq!(snapshot(&second), before_second);

    let raw = stdout(&first_output);
    assert!(!raw.contains(&first.to_string_lossy().to_string()));
    assert!(!raw.contains(&second.to_string_lossy().to_string()));
    let plan: Value = serde_json::from_str(raw.trim()).unwrap();
    assert_eq!(plan["schema"], "zed.nix-export-plan/v1");
    assert_eq!(plan["package"]["org"], "acme");
    assert_eq!(plan["package"]["name"], "dataset");
    assert_eq!(plan["package_class"], "data");
    assert_eq!(plan["intent"]["attribute"], "dataset");
    assert_eq!(
        plan["intent"]["systems"],
        serde_json::json!(["aarch64-linux", "x86_64-linux"])
    );
    assert_eq!(plan["intent"]["outputs"], serde_json::json!(["out"]));
    assert_eq!(plan["source"]["manifest_sha256"], sha256(manifest.as_bytes()));
    assert_eq!(
        plan["source"]["lock_sha256"],
        sha256(b"version = 1\n")
    );
    assert_eq!(
        plan["source"]["artifact"]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(plan["dependencies"], serde_json::json!([]));
    assert_eq!(plan["policy"]["profile"], "strict-v1");
    assert_eq!(plan["policy"]["builder_network"], "disabled");
}

#[test]
fn environment_flags_work_without_leaking_or_creating_global_state() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    let home = root.path().join("global-home-must-not-exist");
    let secret = "do-not-serialize-this-secret";
    write_project(
        &project,
        &data_manifest(""),
        Some("version = 1\n"),
        &[("data/value.txt", b"payload")],
    );

    let output = run_plan(
        &project,
        &[],
        &[
            ("ZED_PKG_FROZEN", "yes"),
            ("ZED_PKG_NIX_PLAN_JSON", "on"),
            ("ZED_PKG_HOME", home.to_str().unwrap()),
            ("ZED_PKG_TOKEN", secret),
            ("ZED_PKG_SUPABASE_KEY", secret),
            ("ZED_PKG_REGISTRY", "https://person:secret@example.invalid/registry"),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let raw = stdout(&output);
    assert!(!raw.contains(secret));
    assert!(!raw.contains("person:secret"));
    assert!(!raw.contains(&home.to_string_lossy().to_string()));
    assert!(!home.exists());
}

#[test]
fn exact_manifest_bytes_participate_in_the_plan_identity() {
    let root = TempDir::new().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    let base = data_manifest("");
    let commented = format!("# exact input comment\n{base}");
    write_project(
        &first,
        &base,
        Some("version = 1\n"),
        &[("data/value.txt", b"payload")],
    );
    write_project(
        &second,
        &commented,
        Some("version = 1\n"),
        &[("data/value.txt", b"payload")],
    );

    let first_output = run_plan(&first, &["--frozen", "--json"], &[]);
    let second_output = run_plan(&second, &["--frozen", "--json"], &[]);
    assert!(first_output.status.success(), "{}", stderr(&first_output));
    assert!(second_output.status.success(), "{}", stderr(&second_output));
    let first_plan: Value = serde_json::from_str(stdout(&first_output).trim()).unwrap();
    let second_plan: Value = serde_json::from_str(stdout(&second_output).trim()).unwrap();
    assert_ne!(
        first_plan["source"]["manifest_sha256"],
        second_plan["source"]["manifest_sha256"]
    );
    assert_ne!(
        first_plan["source"]["artifact"]["sha256"],
        second_plan["source"]["artifact"]["sha256"]
    );
}

#[test]
fn prebuilt_bins_must_exist_and_survive_publish_excludes() {
    let root = TempDir::new().unwrap();
    let valid = root.path().join("valid");
    let excluded = root.path().join("excluded");
    let missing = root.path().join("missing");
    let manifest = format!(
        "{}\n[bin]\ntool = \"bin/tool\"\n",
        data_manifest("")
    );
    write_project(
        &valid,
        &manifest,
        Some("version = 1\n"),
        &[("bin/tool", b"#!/bin/sh\necho ok\n")],
    );
    let valid_output = run_plan(&valid, &["--frozen", "--json"], &[]);
    assert!(valid_output.status.success(), "{}", stderr(&valid_output));
    let valid_plan: Value = serde_json::from_str(stdout(&valid_output).trim()).unwrap();
    assert_eq!(valid_plan["package_class"], "prebuilt-bin");
    assert_eq!(valid_plan["bins"]["tool"], "bin/tool");

    let excluded_manifest = format!(
        "{}\n[bin]\ntool = \"bin/tool\"\n\n[publish]\nexclude = [\"bin/**\"]\n",
        data_manifest("")
    );
    write_project(
        &excluded,
        &excluded_manifest,
        Some("version = 1\n"),
        &[("bin/tool", b"#!/bin/sh\necho hidden\n")],
    );
    let excluded_output = run_plan(&excluded, &["--frozen", "--json"], &[]);
    assert!(!excluded_output.status.success());
    assert!(stderr(&excluded_output).contains("absent from the immutable artifact"));

    write_project(&missing, &manifest, Some("version = 1\n"), &[]);
    let missing_output = run_plan(&missing, &["--frozen", "--json"], &[]);
    assert!(!missing_output.status.success());
    assert!(stderr(&missing_output).contains("not a file in the selected package source"));
}

#[test]
fn missing_lock_dependencies_and_build_hooks_fail_closed() {
    let root = TempDir::new().unwrap();
    let missing_lock = root.path().join("missing-lock");
    write_project(
        &missing_lock,
        &data_manifest(""),
        None,
        &[("data/value.txt", b"payload")],
    );
    let output = run_plan(&missing_lock, &["--frozen", "--json"], &[]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("requires existing lockfile"));

    let dependency = root.path().join("dependency");
    let dependency_manifest = format!(
        "{}\n[dependencies]\n\"acme/other\" = \"^1.0\"\n",
        data_manifest("")
    );
    write_project(
        &dependency,
        &dependency_manifest,
        Some("version = 1\n"),
        &[("data/value.txt", b"payload")],
    );
    let output = run_plan(&dependency, &["--frozen", "--json"], &[]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("dependency-free packages only"));

    let build = root.path().join("build");
    let build_manifest = format!(
        "{}\n[build]\ncommand = \"cargo build --release\"\n",
        data_manifest("")
    );
    write_project(
        &build,
        &build_manifest,
        Some("version = 1\n"),
        &[("data/value.txt", b"payload")],
    );
    let output = run_plan(&build, &["--frozen", "--json"], &[]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("does not infer source builds"));
}

#[test]
fn polyglot_target_is_explicit_re_rooted_and_deterministic() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("polyglot");
    let manifest = r#"[package]
org = "acme"
name = "clients"
version = "2.0.0"
description = "polyglot clients"

[package.repository]
url = "https://github.com/acme/clients"

[targets.nodejs]
dir = "clients/node"
adapter = "node"

[targets.nodejs.nix]
attribute = "clients-node"
systems = ["x86_64-linux", "aarch64-linux"]
outputs = ["out"]

[targets.rust]
dir = "clients/rust"
adapter = "rust"

[targets.rust.nix]
attribute = "clients-rust"
systems = ["x86_64-linux"]
outputs = ["out"]
"#;
    write_project(
        &project,
        manifest,
        Some("version = 1\n"),
        &[
            ("clients/node/package.json", b"{\"name\":\"@acme/clients\"}\n"),
            ("clients/node/src/index.js", b"export const value = 1;\n"),
            ("clients/rust/Cargo.toml", b"[package]\nname='clients'\nversion='2.0.0'\n"),
            ("clients/rust/src/lib.rs", b"pub const VALUE: u8 = 1;\n"),
        ],
    );

    let missing_target = run_plan(&project, &["--frozen", "--json"], &[]);
    assert!(!missing_target.status.success());
    assert!(stderr(&missing_target).contains("requires --target"));

    let first = run_plan(
        &project,
        &["--frozen", "--json", "--target", "node"],
        &[],
    );
    let second = run_plan(
        &project,
        &["--frozen", "--json", "--target", "nodejs"],
        &[],
    );
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(stdout(&first), stdout(&second));
    let plan: Value = serde_json::from_str(stdout(&first).trim()).unwrap();
    assert_eq!(plan["package"]["name"], "clients-nodejs");
    assert_eq!(plan["package"]["target"], "nodejs");
    assert_eq!(plan["intent"]["attribute"], "clients-node");
    assert_eq!(
        plan["source"]["file_name"],
        "acme-clients-nodejs-2.0.0.tar.gz"
    );
}

#[test]
fn missing_route_unknown_target_and_non_frozen_mode_are_actionable() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    write_project(
        &project,
        r#"[package]
org = "acme"
name = "plain"
version = "1.0.0"

[package.repository]
url = "https://github.com/acme/plain"
"#,
        Some("version = 1\n"),
        &[("value.txt", b"payload")],
    );

    let non_frozen = run_plan(&project, &["--json"], &[]);
    assert!(!non_frozen.status.success());
    assert!(stderr(&non_frozen).contains("frozen-only"));

    let missing_route = run_plan(&project, &["--frozen", "--json"], &[]);
    assert!(!missing_route.status.success());
    assert!(stderr(&missing_route).contains("declares no [publish.nix]"));

    let unknown_target = run_plan(
        &project,
        &["--frozen", "--json", "--target", "java"],
        &[],
    );
    assert!(!unknown_target.status.success());
    assert!(stderr(&unknown_target).contains("has no target"));
}

#[test]
fn unknown_flags_and_nested_help_are_wired_through_the_modular_parser() {
    let root = TempDir::new().unwrap();
    let project = root.path().join("project");
    write_project(
        &project,
        &data_manifest(""),
        Some("version = 1\n"),
        &[("data/value.txt", b"payload")],
    );

    let unknown = run_plan(&project, &["--surprise"], &[]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("unknown"));

    let help = Command::new(zed())
        .current_dir(&project)
        .env_clear()
        .args(["interop", "nix", "plan", "export", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success(), "{}", stderr(&help));
    let text = stdout(&help);
    assert!(text.contains("--frozen"));
    assert!(text.contains("--json"));
    assert!(text.contains("--target"));
}
