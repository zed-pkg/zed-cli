//! Failure modes of the shared local-project index.
//!
//! The index is operator-owned state, but it is read on every install and its
//! contents become filesystem paths and symlink targets. Everything here is a
//! way that state can be wrong — concurrent writers, hand-edited or hostile
//! files, a swapped index — and asserts that Zed fails closed rather than
//! resolving something it cannot justify.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

fn command(cwd: &Path, home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command
        .current_dir(cwd)
        .args(args)
        .env("ZED_PKG_HOME", home)
        .env("ZED_PKG_REGISTRY", "https://registry.invalid")
        .env_remove("ZED_PKG_LOCAL_REGISTRY")
        .env_remove("ZED_PKG_LOCAL_REGISTRY_FILE")
        .env_remove("ZED_PKG_INTERACTIVE")
        .env_remove("ZED_PKG_TOKEN");
    command
}

fn run(cwd: &Path, home: &Path, args: &[&str]) -> Output {
    command(cwd, home, args).output().unwrap()
}

fn spawn(cwd: &Path, home: &Path, args: &[&str]) -> Child {
    command(cwd, home, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn project(dir: &Path, org: &str, name: &str, version: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join(".zpkg.toml"),
        format!(
            "[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"{version}\"\n\n\
             [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/{org}/{name}\"\n"
        ),
    )
    .unwrap();
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let home = root.join("zed-home");
    fs::create_dir_all(&home).unwrap();
    Fixture {
        _temp: temp,
        root,
        home,
    }
}

impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        run(&self.root, &self.home, args)
    }

    fn index(&self) -> PathBuf {
        PathBuf::from(stdout(&self.run(&["local", "path"])).trim())
    }

    fn write_index(&self, contents: &str) {
        let path = self.index();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
    }
}

/// Sixteen processes registering different projects at once must all survive.
/// A plain read-modify-write would last-writer-wins most of these away.
#[test]
fn concurrent_registrations_do_not_lose_entries() {
    let fx = fixture();
    let count = 16;
    let mut names = Vec::new();
    for index in 0..count {
        let name = format!("pkg-{index:02}");
        project(&fx.root.join(&name), "acme", &name, "1.0.0");
        names.push(name);
    }

    let children: Vec<Child> = names
        .iter()
        .map(|name| spawn(&fx.root, &fx.home, &["local", "register", name]))
        .collect();
    for mut child in children {
        let status = child.wait().unwrap();
        assert!(status.success(), "a concurrent registration failed");
    }

    let listed = fx.run(&["local", "list", "--json"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let report: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    assert_eq!(
        report.as_array().unwrap().len(),
        count,
        "every concurrent registration must survive:\n{}",
        stdout(&listed)
    );
}

/// Racing writers on one path must converge on a single entry, not duplicates.
#[test]
fn concurrent_registrations_of_one_path_converge() {
    let fx = fixture();
    project(&fx.root.join("library"), "acme", "widget", "1.0.0");

    let children: Vec<Child> = (0..8)
        .map(|_| spawn(&fx.root, &fx.home, &["local", "register", "library"]))
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }

    let listed = fx.run(&["local", "list", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    assert_eq!(report.as_array().unwrap().len(), 1);
}

#[test]
fn an_unknown_schema_is_refused_rather_than_best_effort_parsed() {
    let fx = fixture();
    fx.write_index(r#"{"schema":"zed.local-registry.v2","entries":[]}"#);
    let output = fx.run(&["local", "list"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("unsupported local registry schema"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_relative_entry_path_is_refused() {
    let fx = fixture();
    fx.write_index(
        r#"{"schema":"zed.local-registry.v1","entries":[
             {"org":"acme","name":"widget","version":"1.0.0","path":"relative/widget"}]}"#,
    );
    let output = fx.run(&["local", "list"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not an absolute path"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_invalid_package_identity_is_refused() {
    let fx = fixture();
    fx.write_index(
        r#"{"schema":"zed.local-registry.v1","entries":[
             {"org":"../escape","name":"widget","version":"1.0.0","path":"/tmp/widget"}]}"#,
    );
    let output = fx.run(&["local", "list"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("invalid package identity"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn duplicate_paths_are_refused() {
    let fx = fixture();
    fx.write_index(
        r#"{"schema":"zed.local-registry.v1","entries":[
             {"org":"acme","name":"widget","version":"1.0.0","path":"/tmp/widget"},
             {"org":"acme","name":"other","version":"1.0.0","path":"/tmp/widget"}]}"#,
    );
    let output = fx.run(&["local", "list"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("duplicate entries"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_oversized_index_is_refused_before_it_is_parsed() {
    let fx = fixture();
    let padding = "x".repeat(9 * 1024 * 1024);
    fx.write_index(&format!(
        r#"{{"schema":"zed.local-registry.v1","entries":[],"padding":"{padding}"}}"#
    ));
    let output = fx.run(&["local", "list"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("ceiling"), "{}", stderr(&output));
}

#[cfg(unix)]
#[test]
fn a_symlinked_index_file_is_refused() {
    let fx = fixture();
    let real = fx.root.join("elsewhere.json");
    fs::write(&real, r#"{"schema":"zed.local-registry.v1","entries":[]}"#).unwrap();
    let path = fx.index();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&real, &path).unwrap();

    let output = fx.run(&["local", "list"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("non-symlink"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_empty_index_file_is_treated_as_an_empty_registry() {
    let fx = fixture();
    fx.write_index("");
    let output = fx.run(&["local", "list"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("no local projects registered"));
}

#[test]
fn the_index_location_can_be_relocated_for_hermetic_runs() {
    let fx = fixture();
    let elsewhere = fx.root.join("sandbox").join("index.json");
    project(&fx.root.join("library"), "acme", "widget", "1.0.0");

    let registered = command(&fx.root, &fx.home, &["local", "register", "library"])
        .env("ZED_PKG_LOCAL_REGISTRY_FILE", &elsewhere)
        .output()
        .unwrap();
    assert!(registered.status.success(), "{}", stderr(&registered));
    assert!(elsewhere.is_file(), "the relocated index must be written");
    assert!(
        !fx.index().exists(),
        "the default index must stay untouched"
    );

    // Without the override the registration is invisible.
    assert!(stdout(&fx.run(&["local", "list"])).contains("no local projects registered"));
}

#[test]
fn a_relative_index_override_is_refused() {
    let fx = fixture();
    let output = command(&fx.root, &fx.home, &["local", "list"])
        .env("ZED_PKG_LOCAL_REGISTRY_FILE", "relative/index.json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("must be an absolute path"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn an_unparseable_local_registry_mode_fails_closed() {
    let fx = fixture();
    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0");
    let output = command(&app, &fx.home, &["install"])
        .env("ZED_PKG_LOCAL_REGISTRY", "sometimes")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "an unrecognized mode must never silently become the default"
    );
    assert!(
        stderr(&output).to_lowercase().contains("sometimes"),
        "the rejected value must be named:\n{}",
        stderr(&output)
    );
}

#[test]
fn a_selector_that_matches_nothing_is_an_error() {
    let fx = fixture();
    let output = fx.run(&["local", "unregister", "acme/absent"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no local registry entry matches"),
        "{}",
        stderr(&output)
    );

    let nonsense = fx.run(&["local", "unregister", "Not A Selector"]);
    assert!(!nonsense.status.success());
    assert!(
        stderr(&nonsense).contains("neither an existing directory"),
        "{}",
        stderr(&nonsense)
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_manifest_cannot_claim_a_package_identity() {
    let fx = fixture();
    let real = fx.root.join("real");
    project(&real, "acme", "widget", "1.0.0");
    let impostor = fx.root.join("impostor");
    fs::create_dir_all(&impostor).unwrap();
    std::os::unix::fs::symlink(real.join(".zpkg.toml"), impostor.join(".zpkg.toml")).unwrap();

    let output = fx.run(&["local", "register", "impostor"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("non-symlink"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_project_directory_is_recorded_by_its_real_path() {
    let fx = fixture();
    let real = fx.root.join("real");
    project(&real, "acme", "widget", "1.0.0");
    let alias = fx.root.join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();

    assert!(
        fx.run(&["local", "register", "alias"]).status.success(),
        "registering through a symlinked spelling must work"
    );
    // Registering the real path is the same entry, not a second one.
    assert!(fx.run(&["local", "register", "real"]).status.success());

    let listed = fx.run(&["local", "list", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    let entries = report.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], real.to_str().unwrap());
}

#[test]
fn scan_refuses_a_depth_above_the_ceiling() {
    let fx = fixture();
    let output = fx.run(&["local", "scan", ".", "--max-depth", "64"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("ceiling"), "{}", stderr(&output));
}
