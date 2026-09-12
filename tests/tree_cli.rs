//! End-to-end behavior of `zed tree` and `zed why`.
//!
//! Both commands are offline by construction: they read the manifest, the
//! lockfile, and the materialization directory, and nothing else. These tests
//! build that state by hand rather than by installing, so they assert what the
//! commands *read* rather than what an installer happens to produce.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn zed(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zed"))
        .current_dir(root)
        .args(args)
        .env_remove("ZED_PKG_TREE_DEPTH")
        .env_remove("ZED_PKG_TREE_JSON")
        .env_remove("ZED_PKG_WHY_JSON")
        .env_remove("ZED_PKG_INTERACTIVE")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn assert_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn manifest(org: &str, name: &str, version: &str, dependencies: &[(&str, &str)]) -> String {
    let mut text = format!(
        "[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"{version}\"\n\
         [package.repository]\nvcs = \"git\"\nurl = \"https://localhost/{org}/{name}\"\n"
    );
    if !dependencies.is_empty() {
        text.push_str("\n[dependencies]\n");
        for (key, requirement) in dependencies {
            text.push_str(&format!("\"{key}\" = \"{requirement}\"\n"));
        }
    }
    text
}

/// A materialized package: what `zed install` leaves in `zed_modules`.
fn materialize(root: &Path, org: &str, name: &str, version: &str, deps: &[(&str, &str)]) {
    let dir = root.join("zed_modules").join(org).join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".zpkg.toml"), manifest(org, name, version, deps)).unwrap();
}

fn locked(entries: &[(&str, &str, &str)]) -> String {
    let mut text = String::from("version = 1\n");
    for (org, name, version) in entries {
        text.push_str(&format!(
            "\n[[package]]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"{version}\"\n\
             sha256 = \"{}\"\nsize = 1\nformat = \"tar.gz\"\nvcs_tag = \"v{version}\"\n\
             vcs_commit = \"artifact-sha256:{}\"\nsource = \"file:///dev/null\"\n",
            "a".repeat(64),
            "a".repeat(64),
        ));
    }
    text
}

/// A project whose graph is: app -> left, right; left -> shared; right -> shared.
fn diamond(root: &Path) {
    fs::write(
        root.join(".zpkg.toml"),
        manifest(
            "acme",
            "app",
            "1.0.0",
            &[("acme/left", "^1"), ("acme/right", "^1")],
        ),
    )
    .unwrap();
    fs::write(
        root.join(".zpkg.lock"),
        locked(&[
            ("acme", "left", "1.0.0"),
            ("acme", "right", "1.0.0"),
            ("acme", "shared", "2.0.0"),
        ]),
    )
    .unwrap();
    materialize(root, "acme", "left", "1.0.0", &[("acme/shared", "^2")]);
    materialize(root, "acme", "right", "1.0.0", &[("acme/shared", "^2")]);
    materialize(root, "acme", "shared", "2.0.0", &[]);
}

#[test]
fn the_tree_shows_the_whole_graph_and_marks_the_repeat() {
    let project = tempfile::tempdir().unwrap();
    diamond(project.path());

    let output = zed(project.path(), &["tree"]);
    assert_ok(&output, "zed tree");
    let text = stdout(&output);
    assert!(text.starts_with("acme/app 1.0.0"), "{text}");
    assert!(text.contains("acme/left 1.0.0 (^1)"), "{text}");
    assert!(text.contains("acme/shared 2.0.0 (^2)"), "{text}");
    // One expansion, one back-reference: the same shape `cargo tree` prints.
    assert_eq!(text.matches("acme/shared").count(), 2, "{text}");
    assert_eq!(text.matches("(*)").count(), 1, "{text}");
}

#[test]
fn a_depth_limit_truncates_and_says_so() {
    let project = tempfile::tempdir().unwrap();
    diamond(project.path());

    let output = zed(project.path(), &["tree", "--depth", "1"]);
    assert_ok(&output, "zed tree --depth 1");
    let text = stdout(&output);
    assert!(!text.contains("acme/shared"), "{text}");
    assert_eq!(text.matches("...").count(), 2, "{text}");
}

#[test]
fn the_tree_can_be_rooted_at_one_package() {
    let project = tempfile::tempdir().unwrap();
    diamond(project.path());

    let output = zed(project.path(), &["tree", "acme/left"]);
    assert_ok(&output, "zed tree acme/left");
    let text = stdout(&output);
    assert!(text.starts_with("acme/left 1.0.0"), "{text}");
    assert!(text.contains("acme/shared"), "{text}");
    assert!(!text.contains("acme/right"), "{text}");

    let missing = zed(project.path(), &["tree", "acme/nope"]);
    assert!(!missing.status.success());
}

#[test]
fn the_json_view_is_the_same_graph_in_a_stable_shape() {
    let project = tempfile::tempdir().unwrap();
    diamond(project.path());

    let output = zed(project.path(), &["tree", "--json"]);
    assert_ok(&output, "zed tree --json");
    let document: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(document["schema"], "zed.tree.v1");
    let rows = document["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["coordinate"], "acme/left");
    assert_eq!(rows[1]["state"], "expanded");
    assert_eq!(rows[3]["state"], "repeated");
}

#[test]
fn a_locked_package_that_was_never_materialized_is_reported_as_unreadable() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join(".zpkg.toml"),
        manifest("acme", "app", "1.0.0", &[("acme/widget", "^1")]),
    )
    .unwrap();
    fs::write(
        project.path().join(".zpkg.lock"),
        locked(&[("acme", "widget", "1.0.0")]),
    )
    .unwrap();

    let output = zed(project.path(), &["tree"]);
    assert_ok(&output, "zed tree");
    let text = stdout(&output);
    assert!(text.contains("acme/widget 1.0.0"), "{text}");
    assert!(text.contains("dependencies unknown"), "{text}");
}

#[test]
fn a_project_that_was_never_installed_still_prints_its_direct_edges() {
    let project = tempfile::tempdir().unwrap();
    fs::write(
        project.path().join(".zpkg.toml"),
        manifest("acme", "app", "1.0.0", &[("acme/widget", "^1")]),
    )
    .unwrap();

    let output = zed(project.path(), &["tree"]);
    assert_ok(&output, "zed tree with no lockfile");
    let text = stdout(&output);
    assert!(text.contains("acme/widget (^1)"), "{text}");
    assert!(text.contains("run `zed install`"), "{text}");
}

#[test]
fn why_reports_every_route_to_a_shared_dependency() {
    let project = tempfile::tempdir().unwrap();
    diamond(project.path());

    let output = zed(project.path(), &["why", "acme/shared"]);
    assert_ok(&output, "zed why");
    let text = stdout(&output);
    assert!(text.starts_with("acme/shared 2.0.0"), "{text}");
    assert!(
        text.contains("acme/left depends on acme/shared (^2)"),
        "{text}"
    );
    assert!(
        text.contains("acme/right depends on acme/shared (^2)"),
        "{text}"
    );
}

#[test]
fn why_is_honest_when_nothing_depends_on_the_package() {
    let project = tempfile::tempdir().unwrap();
    diamond(project.path());

    let output = zed(project.path(), &["why", "acme/app"]);
    assert_ok(&output, "zed why acme/app");
    assert!(
        stdout(&output).contains("nothing in this project depends on it"),
        "{}",
        stdout(&output)
    );

    let bad = zed(project.path(), &["why", "widget"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("expected `org/name`"),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
}

#[test]
fn the_json_explanation_carries_the_paths_shortest_first() {
    let project = tempfile::tempdir().unwrap();
    diamond(project.path());

    let output = zed(project.path(), &["why", "acme/shared", "--json"]);
    assert_ok(&output, "zed why --json");
    let document: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(document["schema"], "zed.why.v1");
    let paths = document["explanation"]["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
    for path in paths {
        assert_eq!(path.as_array().unwrap().len(), 2);
    }
    assert_eq!(document["explanation"]["version"], "2.0.0");
}
