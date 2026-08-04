use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], home: &Path, bin_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(args)
        .env("ZED_PKG_HOME", home)
        .env("ZED_PKG_GLOBAL_BIN_DIR", bin_dir)
        .env_remove("ZED_PKG_TOKEN")
        .output()
        .expect("run zed")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn root_help_advertises_global_package_management() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(&["--help"], home.path(), &bin_dir);

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("global"), "root help omitted global command: {text}");
    assert!(
        text.contains("global-bin-dir"),
        "root help omitted global bin override: {text}"
    );
}

#[test]
fn help_global_lists_the_complete_lifecycle() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(&["help", "global"], home.path(), &bin_dir);

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    for command in ["install", "uninstall", "list", "bin-dir"] {
        assert!(text.contains(command), "global help omitted {command}: {text}");
    }
}

#[test]
fn global_bin_dir_uses_the_explicit_environment_override() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("custom-path-bin");
    let output = run(&["global", "bin-dir"], home.path(), &bin_dir);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), bin_dir.display().to_string());
}

#[test]
fn empty_global_list_is_network_free_and_deterministic() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(&["global", "list"], home.path(), &bin_dir);

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("no global package profiles installed"));
    assert!(text.contains(&bin_dir.display().to_string()));
}

#[test]
fn install_global_alias_rejects_an_invalid_identity_before_network_io() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(
        &["install", "--global", "not-a-package-identity"],
        home.path(),
        &bin_dir,
    );

    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(
        error.contains("invalid package spec"),
        "unexpected error for invalid global package identity: {error}"
    );
}

#[test]
fn uninstall_global_alias_rejects_versioned_selectors() {
    let home = tempfile::tempdir().expect("temporary home");
    let bin_dir = home.path().join("bin");
    let output = run(
        &["uninstall", "--global", "acme/tool@1.0.0"],
        home.path(),
        &bin_dir,
    );

    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(
        error.contains("selectors must be package identities without versions"),
        "unexpected versioned-selector error: {error}"
    );
}
