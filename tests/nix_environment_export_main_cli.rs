use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn zed() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn staged() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed-env-export"))
}

fn write_plan(root: &Path, platform: &str) {
    fs::create_dir_all(root.join(".zed")).unwrap();
    let plan = serde_json::json!({
        "schema": 1,
        "tools": {
            "node": {
                "requirement": "^22",
                "resolved": "22.11.0",
                "provider": "nixpkgs",
                "backend": "nodejs_22",
                "checksums": [
                    {"algorithm": "sha256", "value": "a".repeat(64)}
                ],
                "platforms": []
            }
        },
        "system-packages": {
            "git": {
                "requirement": "2.47.0",
                "resolved": "2.47.0",
                "provider": "nixpkgs",
                "package_ref": "gitFull",
                "checksums": [
                    {"algorithm": "sha256", "value": "b".repeat(64)}
                ],
                "platforms": [platform]
            }
        },
        "platforms": [platform],
        "activation": "frozen-install",
        "sources": []
    });
    fs::write(
        root.join(".zed/environment-plan.json"),
        serde_json::to_vec_pretty(&plan).unwrap(),
    )
    .unwrap();
}

fn clean_command(binary: &Path, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .env_remove("ZED_PKG_ENV_PLAN")
        .env_remove("ZED_PKG_ENV_OUTPUT")
        .env_remove("ZED_PKG_ENV_OUT")
        .env_remove("ZED_PKG_ENV_RECEIPT")
        .env_remove("ZED_PKG_ENV_CHECK")
        .env_remove("ZED_PKG_ENV_WRITE")
        .env_remove("ZED_PKG_ENV_JSON")
        .env_remove("ZED_PKG_UPDATE_CHECK");
    command
}

fn run_main(root: &Path, manager: &str, args: &[&str]) -> Output {
    clean_command(&zed(), root)
        .args(["env", "export", manager])
        .args(args)
        .output()
        .unwrap()
}

fn run_staged(root: &Path, manager: &str, args: &[&str]) -> Output {
    clean_command(&staged(), root)
        .arg(manager)
        .args(args)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn canonical_and_staged_default_exports_are_byte_identical() {
    for (manager, platform, output, receipt) in [
        (
            "devbox",
            "x86_64-linux",
            "devbox.json",
            ".zed/environment-exports/devbox.json",
        ),
        (
            "flox",
            "aarch64-darwin",
            ".flox/env/manifest.toml",
            ".zed/environment-exports/flox.json",
        ),
    ] {
        let canonical = tempfile::tempdir().unwrap();
        let compatibility = tempfile::tempdir().unwrap();
        write_plan(canonical.path(), platform);
        write_plan(compatibility.path(), platform);

        let canonical_result = run_main(canonical.path(), manager, &["--json"]);
        let staged_result = run_staged(compatibility.path(), manager, &["--json"]);
        assert_success(&canonical_result);
        assert_success(&staged_result);
        assert_eq!(canonical_result.stdout, staged_result.stdout, "{manager}");
        assert_eq!(canonical_result.stderr, staged_result.stderr, "{manager}");
        assert_eq!(
            fs::read(canonical.path().join(output)).unwrap(),
            fs::read(compatibility.path().join(output)).unwrap(),
            "{manager} output"
        );
        assert_eq!(
            fs::read(canonical.path().join(receipt)).unwrap(),
            fs::read(compatibility.path().join(receipt)).unwrap(),
            "{manager} receipt"
        );
    }
}

#[test]
fn canonical_and_staged_custom_paths_are_equivalent() {
    let canonical = tempfile::tempdir().unwrap();
    let compatibility = tempfile::tempdir().unwrap();
    write_plan(canonical.path(), "x86_64-linux");
    write_plan(compatibility.path(), "x86_64-linux");

    let main = run_main(
        canonical.path(),
        "devbox",
        &[
            "--plan",
            ".zed/environment-plan.json",
            "--output",
            "generated/devbox.json",
            "--receipt",
            "generated/devbox.receipt.json",
            "--json",
        ],
    );
    let staged = run_staged(
        compatibility.path(),
        "devbox",
        &[
            "--plan",
            ".zed/environment-plan.json",
            "--out",
            "generated/devbox.json",
            "--receipt",
            "generated/devbox.receipt.json",
            "--json",
        ],
    );
    assert_success(&main);
    assert_success(&staged);
    assert_eq!(main.stdout, staged.stdout);
    assert_eq!(
        fs::read(canonical.path().join("generated/devbox.json")).unwrap(),
        fs::read(compatibility.path().join("generated/devbox.json")).unwrap()
    );
    assert_eq!(
        fs::read(canonical.path().join("generated/devbox.receipt.json")).unwrap(),
        fs::read(compatibility.path().join("generated/devbox.receipt.json")).unwrap()
    );
}

#[test]
fn manager_specific_options_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    write_plan(root.path(), "x86_64-linux");

    let devbox_check = run_main(root.path(), "devbox", &["--check"]);
    assert!(!devbox_check.status.success());
    assert!(String::from_utf8_lossy(&devbox_check.stderr)
        .contains("--check and --write are supported only for mise export"));

    let mise_receipt = run_main(
        root.path(),
        "mise",
        &[
            "--plan",
            ".zed/environment-plan.json",
            "--receipt",
            "receipt.json",
        ],
    );
    assert!(!mise_receipt.status.success());
    assert!(String::from_utf8_lossy(&mise_receipt.stderr)
        .contains("--receipt is supported only for Devbox and Flox export"));

    let asdf = run_main(root.path(), "asdf", &[]);
    assert!(!asdf.status.success());
    assert!(String::from_utf8_lossy(&asdf.stderr).contains("invalid value 'asdf'"));
}

#[test]
fn canonical_environment_flags_select_output_and_receipt() {
    let root = tempfile::tempdir().unwrap();
    write_plan(root.path(), "x86_64-linux");

    let output = clean_command(&zed(), root.path())
        .args(["env", "export", "devbox", "--json"])
        .env("ZED_PKG_ENV_PLAN", ".zed/environment-plan.json")
        .env("ZED_PKG_ENV_OUTPUT", "generated/devbox.json")
        .env("ZED_PKG_ENV_RECEIPT", "generated/devbox.receipt.json")
        .output()
        .unwrap();
    assert_success(&output);
    assert!(root.path().join("generated/devbox.json").is_file());
    assert!(root.path().join("generated/devbox.receipt.json").is_file());
}
