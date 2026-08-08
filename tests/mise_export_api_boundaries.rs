use std::fs;
use std::path::Path;

use zed_cli::mise_export::{MiseExportMode, export_mise};

fn valid_plan_json() -> &'static str {
    r#"{
  "schema": 2,
  "tools": {"node": [{"requirement": "22.4.0"}]},
  "env": {},
  "vars": {},
  "tasks": {},
  "platforms": ["linux-x64"],
  "activation": "none"
}
"#
}

#[test]
fn public_api_rejects_reserved_export_state_as_the_plan() {
    let temp = tempfile::tempdir().unwrap();
    let reserved = temp.path().join(".zed/mise-export-state.json");
    fs::create_dir_all(reserved.parent().unwrap()).unwrap();
    fs::write(&reserved, valid_plan_json()).unwrap();

    let error = export_mise(
        temp.path(),
        Path::new(".zed/mise-export-state.json"),
        Path::new("mise.toml"),
        MiseExportMode::Print,
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("environment plan cannot target reserved export state"),
        "unexpected error: {error:#}"
    );
    assert!(!temp.path().join("mise.toml").exists());
    assert_eq!(fs::read_to_string(reserved).unwrap(), valid_plan_json());
}

#[cfg(unix)]
#[test]
fn public_api_rejects_a_symlink_to_reserved_export_state() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().unwrap();
    let reserved = temp.path().join(".zed/mise-export-state.json");
    fs::create_dir_all(reserved.parent().unwrap()).unwrap();
    fs::write(&reserved, valid_plan_json()).unwrap();
    symlink(&reserved, temp.path().join("plan.json")).unwrap();

    let error = export_mise(
        temp.path(),
        Path::new("plan.json"),
        Path::new("mise.toml"),
        MiseExportMode::Print,
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("environment plan cannot target reserved export state"),
        "unexpected error: {error:#}"
    );
    assert!(!temp.path().join("mise.toml").exists());
}
