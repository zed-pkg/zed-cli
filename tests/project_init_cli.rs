use std::fs;
use std::process::{Command, Output};

fn run(root: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zed"))
        .current_dir(root)
        .args(args)
        .env_remove("ZED_PKG_HOME")
        .env_remove("ZED_PKG_INIT_PROJECT")
        .env_remove("ZED_PKG_ORG")
        .env_remove("ZED_PKG_NAME")
        .env_remove("ZED_PKG_INTERACTIVE")
        .output()
        .unwrap()
}

#[test]
fn init_project_creates_the_directory_and_infers_its_name() {
    let root = tempfile::tempdir().unwrap();
    let output = run(root.path(), &["init", "project", "--org", "example"]);
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let project = root.path().join("project");
    let manifest = fs::read_to_string(project.join(".zpkg.toml")).unwrap();
    assert!(manifest.contains("org = \"example\""));
    assert!(manifest.contains("name = \"project\""));

    let ignore = fs::read_to_string(project.join(".gitignore")).unwrap();
    assert!(ignore.lines().any(|line| line == ".zed/*"));
    assert!(
        ignore
            .lines()
            .any(|line| line == "!.zed/environment.lock.toml")
    );
}

#[test]
fn package_and_cli_requests_are_separate_transactions() {
    let root = tempfile::tempdir().unwrap();
    let output = run(
        root.path(),
        &["install", "example/pkg@1", "--cli", "nodejs"],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("package operands and --cli tools cannot be mixed")
    );
    assert!(!root.path().join(".zed/environment.lock.toml").exists());
}

#[cfg(unix)]
#[test]
fn cli_install_refuses_a_symlinked_project_state_directory() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join(".zed")).unwrap();

    let output = run(
        root.path(),
        &[
            "install",
            "--cli",
            "nodejs",
            "--cli-target",
            "x86_64-unknown-linux-gnu",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("project CLI state directory `.zed` must be a real directory")
    );
    assert!(!outside.path().join("environment.lock.toml").exists());
}

#[cfg(unix)]
#[test]
fn cli_install_refuses_a_symlinked_environment_lock() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    fs::create_dir(root.path().join(".zed")).unwrap();
    std::os::unix::fs::symlink(
        outside.path(),
        root.path().join(".zed/environment.lock.toml"),
    )
    .unwrap();

    let before = fs::read(outside.path()).unwrap();
    let output = run(
        root.path(),
        &[
            "install",
            "--cli",
            "nodejs",
            "--cli-target",
            "x86_64-unknown-linux-gnu",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must be a regular project-owned file")
    );
    assert_eq!(fs::read(outside.path()).unwrap(), before);
}
