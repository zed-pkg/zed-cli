//! End-to-end behavior of `zed local` and of installs resolved from it.
//!
//! These tests never reach the network. The configured registry is an
//! unroutable `file://` path that holds no packages, so any install that
//! succeeds here succeeded *because* the local registry satisfied it — which
//! is the property the feature exists to provide.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn zed(root: &Path, home: &Path, registry: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zed"))
        .current_dir(root)
        .args(args)
        .env("ZED_PKG_HOME", home)
        .env("ZED_PKG_REGISTRY", registry)
        .env_remove("ZED_PKG_LOCAL_REGISTRY")
        .env_remove("ZED_PKG_LOCAL_REGISTRY_FILE")
        .env_remove("ZED_PKG_INTERACTIVE")
        .env_remove("ZED_PKG_TOKEN")
        .env_remove("ZED_PKG_FROZEN")
        .output()
        .unwrap()
}

fn assert_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Write a minimal, valid project. `dependencies` is a list of
/// `("org/name", "requirement")` pairs.
fn project(dir: &Path, org: &str, name: &str, version: &str, dependencies: &[(&str, &str)]) {
    fs::create_dir_all(dir).unwrap();
    let mut manifest = format!(
        "[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"{version}\"\n\n\
         [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/{org}/{name}\"\n"
    );
    if !dependencies.is_empty() {
        manifest.push_str("\n[dependencies]\n");
        for (key, requirement) in dependencies {
            manifest.push_str(&format!("\"{key}\" = \"{requirement}\"\n"));
        }
    }
    fs::write(dir.join(".zpkg.toml"), manifest).unwrap();
    fs::write(dir.join("marker.txt"), format!("{org}/{name}@{version}\n")).unwrap();
}

/// A syntactically valid registry URL that resolves to an empty directory, so
/// every remote lookup fails and only local resolution can succeed.
fn empty_registry(root: &Path) -> String {
    let dir = root.join("empty-registry");
    fs::create_dir_all(&dir).unwrap();
    format!("file://{}", dir.display())
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    registry: String,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    // Canonicalize: macOS hands out `/var/...` temp paths that canonicalize to
    // `/private/var/...`, and the index stores canonical paths.
    let root = fs::canonicalize(temp.path()).unwrap();
    let home = root.join("zed-home");
    fs::create_dir_all(&home).unwrap();
    let registry = empty_registry(&root);
    Fixture {
        _temp: temp,
        root,
        home,
        registry,
    }
}

impl Fixture {
    fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        zed(cwd, &self.home, &self.registry, args)
    }
}

#[test]
fn register_list_and_unregister_round_trip() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);

    let registered = fx.run(&fx.root, &["local", "register", "library"]);
    assert_ok(&registered, "zed local register");
    assert!(stdout(&registered).contains("acme/widget@1.2.0"));

    let listed = fx.run(&fx.root, &["local", "list", "--json"]);
    assert_ok(&listed, "zed local list --json");
    let report: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    let entries = report.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["package"], "acme/widget");
    assert_eq!(entries[0]["selectable"], true);
    assert_eq!(entries[0]["path"], library.to_str().unwrap());

    // Registering the same directory again refreshes rather than duplicating.
    assert_ok(
        &fx.run(&library, &["local", "register"]),
        "re-register from inside the project",
    );
    let listed = fx.run(&fx.root, &["local", "list", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    assert_eq!(report.as_array().unwrap().len(), 1);

    let removed = fx.run(&fx.root, &["local", "unregister", "library"]);
    assert_ok(&removed, "zed local unregister");
    let listed = fx.run(&fx.root, &["local", "list"]);
    assert!(stdout(&listed).contains("no local projects registered"));
}

#[test]
fn registering_a_directory_without_a_manifest_fails_closed() {
    let fx = fixture();
    let plain = fx.root.join("not-a-project");
    fs::create_dir_all(&plain).unwrap();
    let output = fx.run(&fx.root, &["local", "register", "not-a-project"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains(".zpkg.toml"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_index_lives_under_zed_home_and_is_reported() {
    let fx = fixture();
    let output = fx.run(&fx.root, &["local", "path"]);
    assert_ok(&output, "zed local path");
    let reported = stdout(&output);
    assert!(reported.contains("local-registry"), "{reported}");
    assert!(reported.contains("index.json"), "{reported}");
}

#[test]
fn scan_registers_every_project_below_a_root() {
    let fx = fixture();
    let tree = fx.root.join("tree");
    project(&tree, "acme", "root", "1.0.0", &[]);
    project(
        &tree.join("packages").join("core"),
        "acme",
        "core",
        "1.0.0",
        &[],
    );
    project(
        &tree.join("node_modules").join("vendored"),
        "acme",
        "vendored",
        "1.0.0",
        &[],
    );

    let scanned = fx.run(&fx.root, &["local", "scan", "tree"]);
    assert_ok(&scanned, "zed local scan");
    let listed = stdout(&fx.run(&fx.root, &["local", "list"]));
    assert!(listed.contains("acme/root"), "{listed}");
    assert!(listed.contains("acme/core"), "{listed}");
    assert!(
        !listed.contains("acme/vendored"),
        "materialized dependency trees must not be scanned: {listed}"
    );
}

#[cfg(unix)]
#[test]
fn install_links_a_registered_project_with_no_registry_available() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(
        &fx.run(&fx.root, &["local", "register", "library"]),
        "register the dependency",
    );

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);

    let installed = fx.run(&app, &["install"]);
    assert_ok(&installed, "install resolved from the local registry");
    assert!(
        stdout(&installed).contains("local acme/widget@1.2.0"),
        "the install must say where the package came from:\n{}",
        stdout(&installed)
    );

    let linked = app.join("zed_modules").join("acme").join("widget");
    let metadata = fs::symlink_metadata(&linked).unwrap();
    assert!(
        metadata.file_type().is_symlink(),
        "symlink mode must produce a symlink, not a copy"
    );
    assert_eq!(fs::read_link(&linked).unwrap(), library);

    // The link is live: an edit in the checkout is visible to the consumer
    // without reinstalling. That is the whole point of source linking.
    fs::write(library.join("marker.txt"), "edited\n").unwrap();
    assert_eq!(
        fs::read_to_string(linked.join("marker.txt")).unwrap(),
        "edited\n"
    );
}

#[cfg(unix)]
#[test]
fn transitive_dependencies_of_a_local_project_resolve_locally_too() {
    let fx = fixture();
    let core = fx.root.join("core");
    project(&core, "acme", "core", "1.0.0", &[]);
    let widget = fx.root.join("widget");
    project(&widget, "acme", "widget", "1.2.0", &[("acme/core", "^1")]);
    for name in ["core", "widget"] {
        assert_ok(
            &fx.run(&fx.root, &["local", "register", name]),
            "register a dependency",
        );
    }

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let installed = fx.run(&app, &["install"]);
    assert_ok(&installed, "install a transitive local graph");

    for name in ["widget", "core"] {
        let linked = app.join("zed_modules").join("acme").join(name);
        assert!(
            fs::symlink_metadata(&linked)
                .unwrap()
                .file_type()
                .is_symlink(),
            "{name} must be linked from source"
        );
    }
}

#[test]
fn copy_mode_produces_a_standalone_tree_from_a_local_project() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(
        &fx.run(&fx.root, &["local", "register", "library"]),
        "register the dependency",
    );

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    assert_ok(
        &fx.run(&app, &["install", "--install-mode", "copy"]),
        "copy-mode install from the local registry",
    );

    let materialized = app.join("zed_modules").join("acme").join("widget");
    let metadata = fs::symlink_metadata(&materialized).unwrap();
    assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
    assert!(materialized.join("marker.txt").is_file());
}

#[test]
fn a_requirement_the_local_checkout_cannot_satisfy_falls_through() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(
        &fx.run(&fx.root, &["local", "register", "library"]),
        "register the dependency",
    );

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^9")]);
    let output = fx.run(&app, &["install"]);
    assert!(
        !output.status.success(),
        "the empty registry cannot satisfy ^9 either"
    );
    assert!(
        stderr(&output).contains("no registration satisfies"),
        "the fall-through must be explained:\n{}",
        stderr(&output)
    );
}

#[test]
fn local_registry_off_ignores_registrations_entirely() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(
        &fx.run(&fx.root, &["local", "register", "library"]),
        "register the dependency",
    );

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let output = fx.run(&app, &["install", "--local-registry", "off"]);
    assert!(
        !output.status.success(),
        "with the local registry off the empty remote registry must be consulted"
    );
    assert!(!app.join("zed_modules").join("acme").join("widget").exists());
}

#[test]
fn local_registry_only_names_the_dependency_it_cannot_satisfy() {
    let fx = fixture();
    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let output = fx.run(&app, &["install", "--local-registry", "only"]);
    assert!(!output.status.success());
    let rendered = stderr(&output);
    assert!(rendered.contains("--local-registry=only"), "{rendered}");
    assert!(rendered.contains("acme/widget"), "{rendered}");
}

#[test]
fn two_equal_checkouts_of_one_package_fail_closed_until_a_priority_breaks_the_tie() {
    let fx = fixture();
    let left = fx.root.join("left");
    let right = fx.root.join("right");
    project(&left, "acme", "widget", "1.2.0", &[]);
    project(&right, "acme", "widget", "1.2.0", &[]);
    for name in ["left", "right"] {
        assert_ok(
            &fx.run(&fx.root, &["local", "register", name]),
            "register a checkout",
        );
    }

    let ambiguous = fx.run(&fx.root, &["local", "resolve", "acme/widget"]);
    assert!(!ambiguous.status.success());
    assert!(
        stderr(&ambiguous).contains("cannot choose between"),
        "{}",
        stderr(&ambiguous)
    );

    assert_ok(
        &fx.run(
            &fx.root,
            &["local", "register", "right", "--priority", "10"],
        ),
        "break the tie",
    );
    let resolved = fx.run(&fx.root, &["local", "resolve", "acme/widget", "--json"]);
    assert_ok(&resolved, "zed local resolve");
    let report: serde_json::Value = serde_json::from_str(&stdout(&resolved)).unwrap();
    assert_eq!(report["resolved"]["path"], right.to_str().unwrap());
}

#[test]
fn a_key_selector_matching_several_checkouts_requires_all() {
    let fx = fixture();
    for name in ["left", "right"] {
        let dir = fx.root.join(name);
        project(&dir, "acme", "widget", "1.2.0", &[]);
        assert_ok(
            &fx.run(&fx.root, &["local", "register", name]),
            "register a checkout",
        );
    }

    let refused = fx.run(&fx.root, &["local", "unregister", "acme/widget"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("--all"), "{}", stderr(&refused));

    assert_ok(
        &fx.run(&fx.root, &["local", "unregister", "acme/widget", "--all"]),
        "unregister both checkouts",
    );
    assert!(stdout(&fx.run(&fx.root, &["local", "list"])).contains("no local projects registered"));
}

#[test]
fn disabled_entries_are_kept_but_never_selected() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(
        &fx.run(&fx.root, &["local", "register", "library"]),
        "register the dependency",
    );
    assert_ok(
        &fx.run(&fx.root, &["local", "disable", "library"]),
        "disable the registration",
    );

    let resolved = fx.run(&fx.root, &["local", "resolve", "acme/widget"]);
    assert_ok(&resolved, "resolve with everything disabled");
    assert!(stdout(&resolved).contains("no registered local project satisfies"));

    // Pruning must not forget a deliberately shelved checkout.
    assert_ok(&fx.run(&fx.root, &["local", "prune"]), "prune");
    assert!(stdout(&fx.run(&fx.root, &["local", "list"])).contains("acme/widget"));

    assert_ok(
        &fx.run(&fx.root, &["local", "enable", "library"]),
        "re-enable the registration",
    );
    let resolved = fx.run(&fx.root, &["local", "resolve", "acme/widget"]);
    assert!(stdout(&resolved).contains("satisfied by"));
}

#[test]
fn prune_drops_registrations_whose_checkout_disappeared() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(
        &fx.run(&fx.root, &["local", "register", "library"]),
        "register the dependency",
    );
    fs::remove_dir_all(&library).unwrap();

    let listed = stdout(&fx.run(&fx.root, &["local", "list"]));
    assert!(listed.contains("stale"), "{listed}");

    let planned = fx.run(&fx.root, &["local", "prune", "--dry-run"]);
    assert_ok(&planned, "prune --dry-run");
    assert!(stdout(&planned).contains("would drop"));
    assert!(stdout(&fx.run(&fx.root, &["local", "list"])).contains("acme/widget"));

    assert_ok(&fx.run(&fx.root, &["local", "prune"]), "prune");
    assert!(stdout(&fx.run(&fx.root, &["local", "list"])).contains("no local projects registered"));
}

#[test]
fn a_stale_registration_is_reported_and_skipped_during_install() {
    let fx = fixture();
    let library = fx.root.join("library");
    project(&library, "acme", "widget", "1.2.0", &[]);
    assert_ok(
        &fx.run(&fx.root, &["local", "register", "library"]),
        "register the dependency",
    );
    // The checkout is repurposed for a different package under the index.
    project(&library, "acme", "other", "1.2.0", &[]);

    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/widget", "^1")]);
    let output = fx.run(&app, &["install"]);
    assert!(!output.status.success(), "the empty registry has no widget");
    assert!(
        stderr(&output).contains("unusable"),
        "the broken registration must be named:\n{}",
        stderr(&output)
    );
}

#[test]
fn a_registration_overlapping_the_consumer_is_refused() {
    let fx = fixture();
    let app = fx.root.join("app");
    project(&app, "acme", "app", "0.1.0", &[("acme/inner", "^1")]);
    let inner = app.join("inner");
    project(&inner, "acme", "inner", "1.0.0", &[]);
    assert_ok(
        &fx.run(&fx.root, &["local", "register", "app/inner"]),
        "register a directory inside the consumer",
    );

    let output = fx.run(&app, &["install"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("overlaps the project"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn the_zed_home_directory_cannot_be_registered() {
    let fx = fixture();
    let inside = fx.home.join("store").join("candidate");
    project(&inside, "acme", "widget", "1.0.0", &[]);
    let output = fx.run(&fx.root, &["local", "register", inside.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Zed home directory"),
        "{}",
        stderr(&output)
    );
}
