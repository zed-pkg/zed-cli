use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use zed_cli::mise_export::{MiseExportMode, export_mise};
use zed_cli::project_lock;
use zed_cli::transaction::{ProjectTransaction, STAGING_DIR};

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

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

fn spawn_export(project: &Path, home: &Path) -> Child {
    let empty_path = home.join("empty-path");
    fs::create_dir_all(&empty_path).unwrap();
    let mut command = Command::new(zed_bin());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ZED_PKG_") {
            command.env_remove(key);
        }
    }
    command
        .args([
            "env",
            "export",
            "mise",
            "--plan",
            "plan.json",
            "--output",
            "mise.toml",
            "--write",
            "--json",
        ])
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ZED_PKG_HOME", home.join(".zed-pkg"))
        .env("PATH", empty_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn assert_child_success(child: Child) {
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "export failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

#[test]
fn public_write_waits_for_checkout_operation_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(root.join("plan.json"), valid_plan_json()).unwrap();

    let child = project_lock::with_lock(&root, "hold exporter test lock", || {
        let mut child = spawn_export(&root, &home);
        thread::sleep(Duration::from_millis(350));
        assert!(
            child.try_wait().unwrap().is_none(),
            "export completed while another process owned the checkout operation lock"
        );
        Ok(child)
    })
    .unwrap();

    assert_child_success(child);
    assert!(root.join("mise.toml").is_file());
    assert!(root.join(".zed/mise-export-state.json").is_file());
}

#[test]
fn startup_recovery_waits_for_checkout_operation_lock() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    fs::write(root.join("plan.json"), valid_plan_json()).unwrap();

    let sentinel = root.join("sentinel.txt");
    fs::write(&sentinel, "before").unwrap();

    let child = project_lock::with_lock(&root, "hold recovery test lock", || {
        let mut transaction = ProjectTransaction::begin(&root).unwrap();
        transaction.backup(&sentinel).unwrap();
        fs::write(&sentinel, "partial").unwrap();
        std::mem::forget(transaction);

        let mut child = spawn_export(&root, &home);
        thread::sleep(Duration::from_millis(350));
        assert!(
            child.try_wait().unwrap().is_none(),
            "startup recovery completed while another process owned the checkout operation lock"
        );
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "partial");
        assert!(root.join(STAGING_DIR).is_dir());
        Ok(child)
    })
    .unwrap();

    assert_child_success(child);
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "before");
    assert!(!root.join(STAGING_DIR).exists());
    assert!(root.join("mise.toml").is_file());
}
