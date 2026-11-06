//! Read-only AttoLang import paths: true CLI process, no registry or compiler.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn invoke(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zed"))
        .current_dir(cwd)
        .args(args)
        .env_remove("ZED_PKG_TOKEN")
        .env_remove("ZED_PKG_REGISTRY")
        .env_remove("ZED_PKG_ATTOLANG_JSON")
        .output()
        .expect("launch zed")
}

fn add_project(root: &Path) {
    fs::create_dir_all(root.join(".zed")).unwrap();
    fs::write(
        root.join(".ores.pkg.toml"),
        "[package]\nname = 'consumer'\n",
    )
    .unwrap();
}

fn index(root: &Path, entries: serde_json::Value) {
    fs::write(
        root.join(".zed/paths.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "modules_dir": "zed_modules",
            "packages": entries
        }))
        .unwrap(),
    )
    .unwrap();
}

fn package(root: &Path, org: &str, name: &str, marker: bool) {
    let dest = root.join("zed_modules").join(org).join(name);
    fs::create_dir_all(&dest).unwrap();
    if marker {
        fs::write(
            dest.join(".ores.pkg.toml"),
            "[package]\nname = 'dependency'\n",
        )
        .unwrap();
    }
}

#[test]
fn paths_are_stable_and_only_include_attolang_packages() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("consumer with spaces");
    add_project(&root);
    package(&root, "acme", "zeta", true);
    package(&root, "acme", "alpha", true);
    package(&root, "acme", "js", false);
    index(
        &root,
        serde_json::json!([
            {"package":"acme/zeta","version":"1.0.0","language":"universal","ecosystem":"universal","path":"zed_modules/acme/zeta"},
            {"package":"acme/js","version":"1.0.0","language":"nodejs","ecosystem":"npm","path":"zed_modules/acme/js"},
            {"package":"acme/alpha","version":"1.0.0","language":"universal","ecosystem":"universal","path":"zed_modules/acme/alpha"}
        ]),
    );
    let before = fs::read(root.join(".zed/paths.json")).unwrap();
    let result = invoke(&root, &["attolang", "paths"]);
    assert!(
        result.status.success(),
        "{:?}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        "zed_modules/acme/alpha:zed_modules/acme/zeta\n"
    );
    let json = invoke(&root, &["oreslang", "paths", "--json"]);
    assert!(
        json.status.success(),
        "{:?}",
        String::from_utf8_lossy(&json.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(doc["env"], "ORES_PATHS");
    assert_eq!(
        doc["paths"],
        serde_json::json!(["zed_modules/acme/alpha", "zed_modules/acme/zeta"])
    );
    assert_eq!(doc["value"], "zed_modules/acme/alpha:zed_modules/acme/zeta");
    assert_eq!(fs::read(root.join(".zed/paths.json")).unwrap(), before);
    assert!(
        !root.join(".zpkg.lock").exists(),
        "export must not write a lockfile"
    );
}

#[test]
fn nested_invocation_projects_paths_relative_to_current_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    package(root, "acme", "math", true);
    index(
        root,
        serde_json::json!([
            {"package":"acme/math","version":"2.0.0","path":"zed_modules/acme/math"}
        ]),
    );
    fs::create_dir_all(root.join("src/nested")).unwrap();
    let result = invoke(&root.join("src/nested"), &["attolang", "paths"]);
    assert!(
        result.status.success(),
        "{:?}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        "../../zed_modules/acme/math\n"
    );
}

#[test]
fn missing_or_poisoned_install_index_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    let missing = invoke(root, &["attolang", "paths"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("run zed install --frozen"));

    package(root, "acme", "math", true);
    for dangerous in [
        "../outside",
        "zed_modules/../outside",
        "/tmp/outside",
        "zed_modules/acme/math:other",
    ] {
        index(
            root,
            serde_json::json!([
                {"package":"acme/math","version":"1.0.0","language":"oreslang","path":dangerous}
            ]),
        );
        let result = invoke(root, &["attolang", "paths"]);
        assert!(!result.status.success(), "accepted {dangerous:?}");
        assert!(
            result.stdout.is_empty(),
            "poisoned paths must not reach stdout"
        );
    }
    index(
        root,
        serde_json::json!([
            {"package":"acme/math","version":"1.0.0","language":"attolang","path":"zed_modules/acme/other"}
        ]),
    );
    let result = invoke(root, &["attolang", "paths"]);
    assert!(!result.status.success(), "accepted foreign package slot");
}

#[test]
fn missing_attolang_marker_does_not_export_unrelated_dependencies() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    package(root, "acme", "node", false);
    index(
        root,
        serde_json::json!([
            {"package":"acme/node","version":"1.0.0","language":"nodejs","path":"zed_modules/acme/node"}
        ]),
    );
    let result = invoke(root, &["attolang", "paths"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("no installed AttoLang packages"));
}

#[cfg(unix)]
#[test]
fn paths_support_store_symlinks_without_following_outside_index_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let store = temp.path().join("immutable-store-entry");
    add_project(&root);
    fs::create_dir_all(&store).unwrap();
    fs::write(store.join(".ores.pkg.toml"), "[package]\nname = 'math'\n").unwrap();
    let slot = root.join("zed_modules/acme/math");
    fs::create_dir_all(slot.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&store, &slot).unwrap();
    index(
        &root,
        serde_json::json!([
            {"package":"acme/math","version":"1.0.0","path":"zed_modules/acme/math"}
        ]),
    );
    let result = invoke(&root, &["attolang", "paths"]);
    assert!(result.status.success(), "{:?}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        "zed_modules/acme/math\n"
    );
}
