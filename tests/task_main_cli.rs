use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn zed() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn zed_task() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed-task"))
}

fn write_plan(root: &Path) {
    fs::write(
        root.join("zed-env.toml"),
        r#"
schema = 2

[env]
MESSAGE = "hello"

[tasks.prepare]
description = "prepare inputs"
run = ["printf 'prepare\\n' > order.txt"]
run_windows = ["echo prepare>order.txt"]

[tasks.build]
description = "build project"
aliases = ["b"]
depends = ["prepare"]
run = ["printf '%s\\n' \"$MESSAGE\" >> order.txt"]
run_windows = ["echo %MESSAGE%>>order.txt"]
"#,
    )
    .unwrap();
}

fn clean_command(binary: &Path, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .current_dir(root)
        .env_remove("ZED_TASK_PLAN")
        .env_remove("ZED_TASK_JSON")
        .env_remove("ZED_TASK_ALL")
        .env_remove("ZED_TASK_DRY_RUN")
        .env_remove("ZED_TASK_YES")
        .env_remove("ZED_TASK_JOBS")
        .env_remove("ZED_TASK_NO_CACHE")
        .env_remove("ZED_PKG_COMMAND")
        .env_remove("ZED_PKG_UNKNOWN_OPTIONS")
        .env_remove("ZED_PKG_PARSE_ERRORS");
    command
}

fn run_main(root: &Path, args: &[&str]) -> Output {
    clean_command(&zed(), root)
        .arg("task")
        .args(args)
        .output()
        .unwrap()
}

fn run_staged(root: &Path, args: &[&str]) -> Output {
    clean_command(&zed_task(), root)
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
fn canonical_and_staged_json_surfaces_are_byte_identical() {
    let root = tempfile::tempdir().unwrap();
    write_plan(root.path());

    for args in [
        &["--json", "list"][..],
        &["--json", "info", "b"],
        &["--json", "graph", "b"],
        &["--json", "run", "b", "--dry-run"],
    ] {
        let main = run_main(root.path(), args);
        let staged = run_staged(root.path(), args);
        assert_success(&main);
        assert_success(&staged);
        assert_eq!(main.stdout, staged.stdout, "stdout mismatch for {args:?}");
        assert_eq!(main.stderr, staged.stderr, "stderr mismatch for {args:?}");
    }
}

#[test]
fn canonical_task_route_executes_the_shared_runtime() {
    let root = tempfile::tempdir().unwrap();
    write_plan(root.path());

    let output = run_main(root.path(), &["run", "b", "--jobs", "2"]);
    assert_success(&output);
    let lines = fs::read_to_string(root.path().join("order.txt")).unwrap();
    let lines = lines.lines().map(str::trim).collect::<Vec<_>>();
    assert_eq!(lines, ["prepare", "hello"]);
}

#[test]
fn both_routes_reject_zero_concurrency_and_live_json() {
    let root = tempfile::tempdir().unwrap();
    write_plan(root.path());

    for run in [run_main as fn(&Path, &[&str]) -> Output, run_staged] {
        let zero = run(root.path(), &["run", "build", "--jobs", "0"]);
        assert!(!zero.status.success());
        assert!(String::from_utf8_lossy(&zero.stderr).contains("at least one"));

        let live_json = run(root.path(), &["--json", "run", "build"]);
        assert!(!live_json.status.success());
        assert!(String::from_utf8_lossy(&live_json.stderr).contains("requires `--dry-run`"));
    }
}

#[test]
fn canonical_route_honors_the_existing_task_environment_contract() {
    let root = tempfile::tempdir().unwrap();
    write_plan(root.path());

    let output = clean_command(&zed(), root.path())
        .arg("task")
        .arg("list")
        .env("ZED_TASK_JSON", "true")
        .env("ZED_TASK_PLAN", "zed-env.toml")
        .output()
        .unwrap();
    assert_success(&output);
    let tasks: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(tasks.as_array().unwrap().len(), 2);
}
