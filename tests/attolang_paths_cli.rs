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
    let native_value = std::env::join_paths(["zed_modules/acme/alpha", "zed_modules/acme/zeta"])
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!("{native_value}\n")
    );
    let json = invoke(&root, &["oreslang", "paths", "--json"]);
    assert!(
        json.status.success(),
        "{:?}",
        String::from_utf8_lossy(&json.stderr)
    );
    let doc: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(doc["env"], "ATTO_PATHS");
    assert_eq!(doc["legacy_env"], "ORES_PATHS");
    assert_eq!(
        doc["paths"],
        serde_json::json!(["zed_modules/acme/alpha", "zed_modules/acme/zeta"])
    );
    assert_eq!(doc["value"], native_value);
    let canonical = invoke(&root, &["attolang", "paths", "--json"]);
    assert!(canonical.status.success());
    let canonical_doc: serde_json::Value = serde_json::from_slice(&canonical.stdout).unwrap();
    assert_eq!(
        canonical_doc, doc,
        "legacy alias must export canonical env authority"
    );
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
    assert!(
        result.status.success(),
        "{:?}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        "zed_modules/acme/math\n"
    );
}

#[test]
fn duplicate_indexed_package_identity_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    package(root, "acme", "math", true);

    // Duplicate entries are never valid, even when both advertise the same
    // path and version. Conflicting versions must not be silently collapsed
    // by the deterministic path set.
    for second_version in ["1.0.0", "2.0.0"] {
        index(
            root,
            serde_json::json!([
                {"package":"acme/math","version":"1.0.0","language":"attolang","path":"zed_modules/acme/math"},
                {"package":"acme/math","version":second_version,"language":"oreslang","path":"zed_modules/acme/math"}
            ]),
        );
        let result = invoke(root, &["attolang", "paths", "--json"]);
        assert!(!result.status.success());
        assert!(
            result.stdout.is_empty(),
            "never print partial package paths"
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("duplicate indexed package acme/math"),
            "{:?}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn oversized_installed_package_index_is_rejected_before_json_parse() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    fs::write(
        root.join(".zed/paths.json"),
        vec![b' '; 8 * 1024 * 1024 + 1],
    )
    .unwrap();
    let result = invoke(root, &["attolang", "paths"]);
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("byte limit"),
        "{:?}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn index_modules_directory_rejects_portability_and_terminal_poisoning() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    package(root, "acme", "math", true);

    for modules in [
        "zed\\modules",
        "zed_modules\t",
        "zed_modules\u{001b}[31m",
        "zed_modules\u{007f}",
        "zed_modules\u{0085}",
        "zed_modules\u{2028}",
        "zed_modules\u{2029}",
        "zed_modules\u{202e}",
        "zed_modules\u{2066}",
        "zed_modules\u{061c}",
    ] {
        let document = serde_json::json!({
            "modules_dir": modules,
            "packages": [{
                "package": "acme/math",
                "version": "1.0.0",
                "language": "attolang",
                "path": format!("{modules}/acme/math")
            }]
        });
        let before = serde_json::to_vec(&document).unwrap();
        fs::write(root.join(".zed/paths.json"), &before).unwrap();

        let result = invoke(root, &["attolang", "paths", "--json"]);
        assert!(!result.status.success(), "accepted modules_dir {modules:?}");
        assert!(result.stdout.is_empty(), "printed untrusted import roots");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("unsafe ORES_PATHS character"),
            "unexpected error for {modules:?}: {:?}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read(root.join(".zed/paths.json")).unwrap(), before);
    }
}

#[test]
fn indexed_package_path_rejects_terminal_and_host_separator_poisoning() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    package(root, "acme", "math", true);

    for path in [
        "zed_modules\\acme\\math",
        "zed_modules/acme/math\t",
        "zed_modules/acme/math\u{001b}[2J",
        "zed_modules/acme/math\u{007f}",
        "zed_modules/acme/math\u{0085}",
        "zed_modules/acme/math\u{2028}",
        "zed_modules/acme/math\u{2029}",
        "zed_modules/acme/math\u{202e}",
        "zed_modules/acme/math\u{2069}",
        "zed_modules/acme/math\u{200f}",
    ] {
        index(
            root,
            serde_json::json!([{
                "package": "acme/math",
                "version": "1.0.0",
                "language": "attolang",
                "path": path
            }]),
        );
        let result = invoke(root, &["oreslang", "paths"]);
        assert!(!result.status.success(), "accepted path {path:?}");
        assert!(result.stdout.is_empty(), "printed untrusted import roots");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("unsafe ORES_PATHS character"),
            "unexpected error for {path:?}: {:?}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn duplicate_json_fields_and_schema_type_confusion_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    for payload in [
        r#"{"modules_dir":"zed_modules","modules_dir":"elsewhere","packages":[]}"#,
        r#"{"modules_dir":"zed_modules","packages":[{"package":"acme/math","package":"acme/other","version":"1.0.0","path":"zed_modules/acme/math"}]}"#,
        r#"{"modules_dir":null,"packages":[]}"#,
        r#"{"modules_dir":"zed_modules","packages":null}"#,
        r#"{"modules_dir":"zed_modules","packages":[{"package":"acme/math","version":1,"path":"zed_modules/acme/math"}]}"#,
    ] {
        fs::write(root.join(".zed/paths.json"), payload).unwrap();
        let result = invoke(root, &["attolang", "paths"]);
        assert!(
            !result.status.success(),
            "accepted malformed index {payload}"
        );
        assert!(
            result.stdout.is_empty(),
            "published paths from invalid schema"
        );
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("invalid Zed package index"),
            "unexpected error for {payload}: {:?}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn later_poisoned_package_cannot_leak_earlier_valid_import_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    add_project(root);
    package(root, "acme", "math", true);
    index(
        root,
        serde_json::json!([
            {"package":"acme/math","version":"1.0.0","language":"attolang","path":"zed_modules/acme/math"},
            {"package":"acme/rogue","version":"1.0.0","language":"attolang","path":"zed_modules/acme/rogue\u{202e}"}
        ]),
    );

    for args in [
        &["attolang", "paths"][..],
        &["attolang", "paths", "--json"][..],
        &["oreslang", "paths", "--json"][..],
    ] {
        let result = invoke(root, args);
        assert!(!result.status.success(), "accepted poisoned later entry");
        assert!(result.stdout.is_empty(), "printed a partial valid prefix");
    }
}

#[test]
fn native_zpkg_project_marker_without_legacy_root_marker_is_supported() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join(".zed")).unwrap();
    fs::write(root.join(".zpkg.toml"), "[package]\nname='consumer'\n").unwrap();
    package(root, "acme", "math", true);
    index(
        root,
        serde_json::json!([
            {"package":"acme/math","version":"1.0.0","path":"zed_modules/acme/math"}
        ]),
    );

    let result = invoke(root, &["attolang", "paths"]);
    assert!(
        result.status.success(),
        "{:?}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        "zed_modules/acme/math\n"
    );
    assert!(
        !root.join(".zpkg.lock").exists(),
        "read-only projection wrote a lock"
    );
}
