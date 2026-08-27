//! Real-binary certification for declared `[tool-dependencies]` (zed-docs 36).
//!
//! The claim under test is the whole reason the feature exists: a project can
//! pin the exact version of a CLI it needs *without* carrying a copy of it.
//! So the assertions are as much about what is absent from the project tree as
//! about what is present in the central store — and the second project pinning
//! the same version must add no bytes anywhere.
//!
//! No network, credentials, shell startup files, or pre-existing state: a
//! temporary `file://` registry and an isolated `ZED_PKG_HOME`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use zed_cli::ops;
use zed_cli::pack;
use zed_cli::registry::{FileRegistry, Registry};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{BIN_DIR, LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};

const SHIM_MARKER: &str = "zed-tool-shim/v1";

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn run_zed(args: &[&str], cwd: &Path, home: &Path, global_bin: &Path, registry: &Path) -> Output {
    let mut command = Command::new(zed_bin());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ZED_PKG_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .current_dir(cwd)
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
    tool_dependencies: &BTreeMap<String, String>,
    bin: Option<(&str, &str)>,
) -> String {
    let mut manifest = format!(
        r#"[package]
org = "{org}"
name = "{name}"
version = "{version}"
description = "tool-dependency clean-room fixture"
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
    if !tool_dependencies.is_empty() {
        manifest.push_str("\n[tool-dependencies]\n");
        for (package, requirement) in tool_dependencies {
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
        manifest_text(org, name, version, dependencies, &BTreeMap::new(), bin),
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

/// A consumer project: one ordinary dependency it links against, one declared
/// tool it only runs.
fn consumer(root: &Path, name: &str, tool_requirement: Option<&str>) -> PathBuf {
    let project = root.join(name);
    fs::create_dir_all(&project).unwrap();
    let mut dependencies = BTreeMap::new();
    dependencies.insert("acme/support".to_string(), "^1.2".to_string());
    let mut tools = BTreeMap::new();
    if let Some(requirement) = tool_requirement {
        tools.insert("acme/lint".to_string(), requirement.to_string());
    }
    fs::write(
        project.join(MANIFEST_FILE),
        manifest_text("acme", name, "0.0.0", &dependencies, &tools, None),
    )
    .unwrap();
    project
}

fn shim_name(command: &str) -> String {
    if cfg!(windows) {
        format!("{command}.cmd")
    } else {
        command.to_string()
    }
}

fn installed_bin_name(command: &str) -> String {
    if cfg!(windows) {
        format!("{command}.exe")
    } else {
        command.to_string()
    }
}

/// Everything published once, for a suite that then only reads it.
struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    registry_root: PathBuf,
    home: PathBuf,
    global_bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().to_path_buf();
        let fixture_root = root.join("fixtures");
        let registry_root = root.join("registry");
        let home = root.join("home");
        let global_bin = root.join("path-bin");
        fs::create_dir_all(&home).unwrap();

        let registry = FileRegistry::new(registry_root.clone());
        publish(
            &registry,
            &fixture_package(
                &fixture_root,
                "acme",
                "support",
                "1.2.3",
                &BTreeMap::new(),
                None,
                &[("lib/support.txt", b"support-v1\n")],
            ),
        );
        // The linter's own transitive dependency: it must end up in the
        // central profile, never in the consumer's tree.
        publish(
            &registry,
            &fixture_package(
                &fixture_root,
                "acme",
                "lint-core",
                "3.0.0",
                &BTreeMap::new(),
                None,
                &[("lib/rules.txt", b"rules-v3\n")],
            ),
        );
        let mut lint_dependencies = BTreeMap::new();
        lint_dependencies.insert("acme/lint-core".to_string(), "^3".to_string());
        publish(
            &registry,
            &fixture_package(
                &fixture_root,
                "acme",
                "lint",
                "9.12.0",
                &lint_dependencies,
                Some(("lint", "bin/lint")),
                &[("bin/lint", b"#!/bin/sh\necho lint-9.12.0 \"$@\"\n")],
            ),
        );

        Self {
            _temporary: temporary,
            root,
            registry_root,
            home,
            global_bin,
        }
    }

    fn zed(&self, args: &[&str], cwd: &Path) -> Output {
        run_zed(args, cwd, &self.home, &self.global_bin, &self.registry_root)
    }

    fn profiles(&self) -> PathBuf {
        self.home.join("zed-home").join("global").join("profiles")
    }

    fn lint_profile(&self, version: &str) -> PathBuf {
        self.profiles().join("acme").join("lint").join(version)
    }
}

#[test]
fn a_declared_tool_is_pinned_centrally_and_never_copied_into_the_project() {
    let fixture = Fixture::new();
    let project = consumer(&fixture.root, "web-app", Some("^9"));

    let install = fixture.zed(&["install"], &project);
    assert!(install.status.success(), "{}", stderr(&install));

    // 1. The pin is in the lockfile, in its own array.
    let lock = Lockfile::parse(&fs::read_to_string(project.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(lock.find_tool("acme", "lint").unwrap().version, "9.12.0");
    assert!(
        lock.find("acme", "lint").is_none(),
        "a tool pin must never appear in the materialized package set"
    );
    assert!(
        lock.find("acme", "support").is_some(),
        "ordinary dependencies are unaffected"
    );

    // 2. Nothing of the tool is in the project tree — not the tool, not its
    //    transitive dependency. This is the npm failure mode the whole design
    //    exists to avoid.
    let modules = project.join(MODULES_DIR);
    assert!(modules.join("acme").join("support").exists());
    assert!(!modules.join("acme").join("lint").exists());
    assert!(!modules.join("acme").join("lint-core").exists());

    // 3. The bytes are in the central store, keyed by resolved version, with
    //    the tool's own transitive graph beside it.
    let profile = fixture.lint_profile("9.12.0");
    assert!(profile.join(".zed-global-profile.json").is_file());
    assert!(
        profile
            .join(MODULES_DIR)
            .join("acme")
            .join("lint-core")
            .exists()
    );

    // 4. PATH is not taken implicitly: declaring a tool in a repository does
    //    not decide what a bare `lint` means in the user's shell.
    assert!(
        !fixture
            .profiles()
            .join("acme")
            .join("lint")
            .join("current")
            .exists()
    );
    assert!(!fixture.global_bin.join(installed_bin_name("lint")).exists());

    // 5. The project gets a shim — a pointer, not a copy.
    let shim = modules.join(BIN_DIR).join(shim_name("lint"));
    let body = fs::read_to_string(&shim).unwrap();
    assert!(body.contains(SHIM_MARKER), "{body}");
    assert!(body.contains("acme/lint@9.12.0"), "{body}");
    assert!(body.len() < 1024, "a shim is bytes, not a tool: {body}");

    let listed = fixture.zed(&["tools", "list"], &project);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let listed_text = stdout(&listed);
    assert!(listed_text.contains("acme/lint"), "{listed_text}");
    assert!(listed_text.contains("9.12.0"), "{listed_text}");
    assert!(listed_text.contains("ready"), "{listed_text}");
}

#[test]
fn a_second_project_pinning_the_same_version_adds_no_bytes() {
    let fixture = Fixture::new();
    let first = consumer(&fixture.root, "first", Some("^9"));
    let second = consumer(&fixture.root, "second", Some("^9.12"));

    assert!(fixture.zed(&["install"], &first).status.success());
    let installed = fixture.zed(&["install"], &second);
    assert!(installed.status.success(), "{}", stderr(&installed));

    let versions: Vec<String> = fs::read_dir(fixture.profiles().join("acme").join("lint"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        versions,
        ["9.12.0"],
        "one central copy serves every project that pins that version"
    );
    assert!(
        !second.join(MODULES_DIR).join("acme").join("lint").exists(),
        "and still nothing lands in the second project"
    );
}

#[test]
fn skip_pins_without_fetching_and_sync_provisions_later() {
    let fixture = Fixture::new();
    let project = consumer(&fixture.root, "ci-app", Some("^9"));

    let install = fixture.zed(&["install", "--tools", "skip"], &project);
    assert!(install.status.success(), "{}", stderr(&install));

    let lock = Lockfile::parse(&fs::read_to_string(project.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert_eq!(
        lock.find_tool("acme", "lint").unwrap().version,
        "9.12.0",
        "--tools=skip still pins: the declaration is the reviewable part"
    );
    assert!(
        !fixture.lint_profile("9.12.0").exists(),
        "...and fetches nothing"
    );
    assert!(
        !project
            .join(MODULES_DIR)
            .join(BIN_DIR)
            .join(shim_name("lint"))
            .exists(),
        "no shim can exist for a tool that is not provisioned"
    );

    let synced = fixture.zed(&["tools", "sync"], &project);
    assert!(synced.status.success(), "{}", stderr(&synced));
    assert!(
        fixture
            .lint_profile("9.12.0")
            .join(".zed-global-profile.json")
            .is_file()
    );
    assert!(
        project
            .join(MODULES_DIR)
            .join(BIN_DIR)
            .join(shim_name("lint"))
            .exists()
    );
}

#[test]
fn dropping_the_declaration_prunes_the_shim() {
    let fixture = Fixture::new();
    let project = consumer(&fixture.root, "shrinking-app", Some("^9"));
    assert!(fixture.zed(&["install"], &project).status.success());
    let shim = project
        .join(MODULES_DIR)
        .join(BIN_DIR)
        .join(shim_name("lint"));
    assert!(shim.exists());

    // Rewrite the manifest without the tool, and reinstall.
    fs::write(
        project.join(MANIFEST_FILE),
        manifest_text(
            "acme",
            "shrinking-app",
            "0.0.0",
            &BTreeMap::from([("acme/support".to_string(), "^1.2".to_string())]),
            &BTreeMap::new(),
            None,
        ),
    )
    .unwrap();
    let reinstall = fixture.zed(&["install"], &project);
    assert!(reinstall.status.success(), "{}", stderr(&reinstall));

    assert!(!shim.exists(), "an undeclared tool keeps no runnable name");
    let lock = Lockfile::parse(&fs::read_to_string(project.join(LOCKFILE_FILE)).unwrap()).unwrap();
    assert!(lock.find_tool("acme", "lint").is_none());
    assert!(
        fixture.lint_profile("9.12.0").exists(),
        "the central copy stays for whoever else pins it; `zed gc` is what reclaims it"
    );
}

/// The shim is a `sh` script, so executing it is a Unix assertion. Everything
/// structural about it is covered above on every platform.
#[cfg(unix)]
#[test]
fn zed_run_executes_the_pinned_version_through_the_shim() {
    let fixture = Fixture::new();
    let project = consumer(&fixture.root, "runnable-app", Some("^9"));
    assert!(fixture.zed(&["install"], &project).status.success());

    let run = fixture.zed(&["run", "lint", "src/"], &project);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(
        stdout(&run).contains("lint-9.12.0 src/"),
        "stdout: {} stderr: {}",
        stdout(&run),
        stderr(&run)
    );

    let which = fixture.zed(&["tools", "which", "lint"], &project);
    assert!(which.status.success(), "{}", stderr(&which));
    let expected = fixture.lint_profile("9.12.0").to_string_lossy().to_string();
    let reported = stdout(&which);
    assert!(
        reported.trim().starts_with(&expected),
        "`zed tools which` must point into the central store: {reported}"
    );

    // Executing the shim directly is the same thing, which is the point: any
    // wrapper already aimed at the project's bin directory reaches the pin.
    let direct = Command::new(project.join(MODULES_DIR).join(BIN_DIR).join("lint"))
        .arg("direct")
        .output()
        .expect("run the shim");
    assert!(direct.status.success(), "{:?}", direct);
    assert!(
        String::from_utf8_lossy(&direct.stdout).contains("lint-9.12.0 direct"),
        "{direct:?}"
    );
}

/// A fresh clone: the lockfile carries the pin, the machine has never fetched
/// it, and `zed run` provisions rather than failing.
#[cfg(unix)]
#[test]
fn zed_run_provisions_a_pin_this_machine_has_never_fetched() {
    let fixture = Fixture::new();
    let project = consumer(&fixture.root, "fresh-clone", Some("^9"));
    assert!(
        fixture
            .zed(&["install", "--tools", "skip"], &project)
            .status
            .success()
    );
    assert!(!fixture.lint_profile("9.12.0").exists());

    let run = fixture.zed(&["run", "lint", "after-clone"], &project);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(
        stdout(&run).contains("lint-9.12.0 after-clone"),
        "stdout: {} stderr: {}",
        stdout(&run),
        stderr(&run)
    );
    assert!(fixture.lint_profile("9.12.0").exists());
}
