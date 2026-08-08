use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn zed_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn write_plan(project: &Path) {
    fs::create_dir_all(project).unwrap();
    fs::write(
        project.join("zed-env.json"),
        r#"{
  "schema": 2,
  "tools": {"node": [{"requirement": "22.4.0"}]},
  "env": {},
  "vars": {},
  "tasks": {},
  "platforms": ["linux-x64"],
  "activation": "none"
}
"#,
    )
    .unwrap();
}

fn run_zed_with_plan(project: &Path, home: &Path, plan: &str, output: &str) -> Output {
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
            "env", "export", "mise", "--plan", plan, "--output", output, "--write",
        ])
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("ZED_PKG_HOME", home.join(".zed-pkg"))
        .env("PATH", empty_path)
        .output()
        .unwrap()
}

fn run_zed(project: &Path, home: &Path, output: &str) -> Output {
    run_zed_with_plan(project, home, "zed-env.json", output)
}

#[test]
fn real_cli_refuses_source_state_and_transaction_staging_destinations() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    fs::create_dir_all(&home).unwrap();
    write_plan(&project);
    let original_plan = fs::read(project.join("zed-env.json")).unwrap();

    for output in [
        "zed-env.json",
        "ZED-ENV.JSON",
        ".zed/mise-export-state.json",
        ".ZED/MISE-EXPORT-STATE.JSON",
        ".zpkg-staging/mise.toml",
        ".ZPKG-STAGING/mise.toml",
    ] {
        let result = run_zed(&project, &home, output);
        assert!(
            !result.status.success(),
            "reserved output unexpectedly succeeded: {output}"
        );
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains("cannot overwrite")
                || stderr.contains("reserved export state")
                || stderr.contains("reserved transaction staging"),
            "unexpected reserved-path error for {output}: {stderr}"
        );
    }

    fs::create_dir_all(project.join(".zed")).unwrap();
    fs::write(project.join(".zed/mise-export-state.json"), &original_plan).unwrap();
    let reserved_source =
        run_zed_with_plan(&project, &home, ".zed/mise-export-state.json", "mise.toml");
    assert!(!reserved_source.status.success());
    let stderr = String::from_utf8_lossy(&reserved_source.stderr);
    assert!(
        stderr.contains("environment plan cannot target reserved export state"),
        "unexpected reserved-source error: {stderr}"
    );
    assert!(!project.join("mise.toml").exists());
    fs::remove_file(project.join(".zed/mise-export-state.json")).unwrap();
    fs::remove_dir(project.join(".zed")).unwrap();

    assert_eq!(
        fs::read(project.join("zed-env.json")).unwrap(),
        original_plan
    );
    assert!(!project.join(".zed/mise-export-state.json").exists());
    assert!(!project.join(".zpkg-staging").exists());
}
