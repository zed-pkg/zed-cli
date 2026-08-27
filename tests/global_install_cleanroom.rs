//! Real-binary certification for the global executable-package lifecycle.
//!
//! The fixture publishes a tool and one transitive library to a temporary
//! `file://` registry, installs through the compatibility spelling, destroys
//! both the registry and materialized profile tree, restores from the exact
//! global-profile lock, and finally uninstalls. No network, credentials, shell
//! startup files, or pre-existing package state are used.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use zed_cli::ops;
use zed_cli::pack;
use zed_cli::registry::{FileRegistry, Registry};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn run_zed(args: &[&str], home: &Path, global_bin: &Path, registry: &Path) -> Output {
    let mut command = Command::new(zed_bin());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ZED_PKG_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ZED_PKG_HOME", home.join("zed-home"))
        .env("ZED_PKG_GLOBAL_BIN_DIR", global_bin)
        .env("ZED_PKG_REGISTRY", format!("file://{}", registry.display()))
        .env("ZED_PKG_INTERACTIVE", "false")
        .output()
        .expect("run zed")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn manifest_text(
    org: &str,
    name: &str,
    version: &str,
    dependencies: &BTreeMap<String, String>,
    bin: Option<(&str, &str)>,
) -> String {
    let mut manifest = format!(
        r#"[package]
org = "{org}"
name = "{name}"
version = "{version}"
description = "global clean-room fixture"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/{org}/{name}"
"#,
    );
    if !dependencies.is_empty() {
        manifest.push_str("\n[dependencies]\n");
        for (package, requirement) in dependencies {
            manifest.push_str(&format!("\"{package}\" = \"{requirement}\"\n"));
        }
    }
    if let Some((command, path)) = bin {
        manifest.push_str(&format!("\n[bin]\n{command} = \"{path}\"\n"));
    }
    manifest
}

fn fixture_package(
    fixture_root: &Path,
    org: &str,
    name: &str,
    version: &str,
    dependencies: &BTreeMap<String, String>,
    bin: Option<(&str, &str)>,
    files: &[(&str, &[u8])],
) -> PathBuf {
    let root = fixture_root.join(format!("{org}-{name}-{version}"));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join(MANIFEST_FILE),
        manifest_text(org, name, version, dependencies, bin),
    )
    .unwrap();
    fs::write(root.join("LICENSE"), "MIT\n").unwrap();
    for (relative, bytes) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    root
}

fn publish(registry: &FileRegistry, project: &Path) {
    let manifest =
        Manifest::parse(&fs::read_to_string(project.join(MANIFEST_FILE)).unwrap()).unwrap();
    let packed = pack::pack(project, &manifest, None).unwrap();
    let metadata = ops::build_publish_meta(&manifest, &packed, Some("fixture-commit".into()));
    registry.publish(&metadata, &packed.path, None).unwrap();
}

#[cfg(windows)]
fn installed_command_name() -> &'static str {
    "acme-tool.exe"
}

#[cfg(not(windows))]
fn installed_command_name() -> &'static str {
    "acme-tool"
}

#[test]
fn global_install_restore_and_uninstall_are_hermetic_and_lock_exact() {
    let temporary = tempfile::tempdir().unwrap();
    let fixture_root = temporary.path().join("fixtures");
    let registry_root = temporary.path().join("registry");
    let home = temporary.path().join("home");
    let global_bin = temporary.path().join("path-bin");
    fs::create_dir_all(&home).unwrap();

    let registry = FileRegistry::new(registry_root.clone());
    let support = fixture_package(
        &fixture_root,
        "acme",
        "support",
        "1.2.3",
        &BTreeMap::new(),
        None,
        &[("lib/support.txt", b"support-v1\n")],
    );
    publish(&registry, &support);

    let mut tool_dependencies = BTreeMap::new();
    tool_dependencies.insert("acme/support".to_string(), "^1.2".to_string());
    let command_bytes = b"global-tool-v1\n";
    let tool = fixture_package(
        &fixture_root,
        "acme",
        "tool",
        "0.1.0",
        &tool_dependencies,
        Some(("acme-tool", "bin/acme-tool")),
        &[("bin/acme-tool", command_bytes)],
    );
    publish(&registry, &tool);

    let install = run_zed(
        &[
            "install",
            "--global",
            "--install-mode",
            "copy",
            "acme/tool@0.1.0",
        ],
        &home,
        &global_bin,
        &registry_root,
    );
    assert!(install.status.success(), "{}", stderr(&install));

    let zed_home = home.join("zed-home");
    // Profiles are keyed by resolved version (zed-docs 36) so one machine can
    // hold the versions several projects pin without them colliding.
    let package_root = zed_home
        .join("global")
        .join("profiles")
        .join("acme")
        .join("tool");
    let profile = package_root.join("0.1.0");
    assert!(
        profile.join(".zed-global-profile.json").is_file(),
        "the install must land in its version directory, not flat under the package"
    );
    assert_eq!(
        fs::read_to_string(package_root.join("current"))
            .unwrap()
            .trim(),
        "0.1.0",
        "`zed global install` is what puts a version on PATH, and says so on disk"
    );
    let lock_path = profile.join(LOCKFILE_FILE);
    let original_lock_text = fs::read_to_string(&lock_path).unwrap();
    let lock = Lockfile::parse(&original_lock_text).unwrap();
    assert_eq!(lock.packages.len(), 2, "direct and transitive packages");
    assert_eq!(lock.find("acme", "tool").unwrap().version, "0.1.0");
    assert_eq!(lock.find("acme", "support").unwrap().version, "1.2.3");
    assert_eq!(
        fs::read(global_bin.join(installed_command_name())).unwrap(),
        command_bytes
    );
    assert!(
        profile
            .join(MODULES_DIR)
            .join("acme")
            .join("support")
            .join("lib/support.txt")
            .is_file()
    );

    let listed = run_zed(&["global", "list"], &home, &global_bin, &registry_root);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let listed_text = stdout(&listed);
    assert!(listed_text.contains("acme/tool@0.1.0"), "{listed_text}");
    assert!(listed_text.contains("bins: acme-tool"), "{listed_text}");

    // Prove lock-only restoration: remove both the registry and every
    // materialized package/command. The immutable store remains under the
    // isolated ZED_PKG_HOME and must be sufficient for --frozen.
    fs::remove_dir_all(&registry_root).unwrap();
    fs::remove_dir_all(profile.join(MODULES_DIR)).unwrap();
    fs::remove_file(global_bin.join(installed_command_name())).unwrap();

    let frozen = run_zed(
        &[
            "global",
            "install",
            "--frozen",
            "--install-mode",
            "copy",
            "acme/tool",
        ],
        &home,
        &global_bin,
        &registry_root,
    );
    assert!(frozen.status.success(), "{}", stderr(&frozen));
    assert_eq!(fs::read_to_string(&lock_path).unwrap(), original_lock_text);
    assert_eq!(
        fs::read(global_bin.join(installed_command_name())).unwrap(),
        command_bytes
    );

    let uninstall = run_zed(
        &["uninstall", "--global", "acme/tool"],
        &home,
        &global_bin,
        &registry_root,
    );
    assert!(uninstall.status.success(), "{}", stderr(&uninstall));
    assert!(!profile.exists());
    assert!(
        !package_root.exists(),
        "removing the last version removes the package root and its PATH marker"
    );
    assert!(!global_bin.join(installed_command_name()).exists());

    let final_list = run_zed(&["global", "list"], &home, &global_bin, &registry_root);
    assert!(final_list.status.success(), "{}", stderr(&final_list));
    assert!(stdout(&final_list).contains("no global package profiles installed"));
}
