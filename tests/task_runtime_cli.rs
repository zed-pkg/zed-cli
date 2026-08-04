use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed-task"))
}

fn write_plan(root: &Path, body: &str) {
    fs::write(root.join("zed-env.toml"), body).unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .current_dir(root)
        .args(args)
        .env_remove("ZED_TASK_PLAN")
        .env_remove("ZED_TASK_JSON")
        .env_remove("ZED_TASK_ALL")
        .env_remove("ZED_TASK_DRY_RUN")
        .env_remove("ZED_TASK_YES")
        .env_remove("ZED_TASK_JOBS")
        .env_remove("ZED_TASK_NO_CACHE")
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
fn list_info_and_graph_are_deterministic_json() {
    let root = tempfile::tempdir().unwrap();
    write_plan(
        root.path(),
        r#"
schema = 2

[tasks.prepare]
description = "prepare inputs"
run = ["echo prepare"]

[tasks.build]
description = "build project"
aliases = ["b"]
depends = ["prepare"]
depends_post = ["cleanup"]
run = ["echo build"]

[tasks.cleanup]
hide = true
run = ["echo cleanup"]
"#,
    );

    let list = run(root.path(), &["--json", "list"]);
    assert_success(&list);
    let list: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["name"], "build");
    assert_eq!(list[1]["name"], "prepare");

    let info = run(root.path(), &["--json", "info", "b"]);
    assert_success(&info);
    let info: Value = serde_json::from_slice(&info.stdout).unwrap();
    assert_eq!(info["name"], "build");
    assert_eq!(info["dependencies"][0], "prepare");
    assert_eq!(info["post_dependencies"][0], "cleanup");

    let graph = run(root.path(), &["--json", "graph", "b"]);
    assert_success(&graph);
    let graph: Value = serde_json::from_slice(&graph.stdout).unwrap();
    assert_eq!(graph["requested"], "b");
    assert_eq!(graph["resolved"], "build");
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 3);
}

#[test]
fn run_orders_dependencies_propagates_scalar_env_and_post_tasks() {
    let root = tempfile::tempdir().unwrap();
    write_plan(
        root.path(),
        r#"
schema = 2

[env]
MESSAGE = "hello"

[tasks.prepare]
run = ["printf 'prepare\\n' > order.txt"]
run_windows = ["echo prepare>order.txt"]

[tasks.build]
depends = ["prepare"]
depends_post = ["cleanup"]
run = ["printf '%s\\n' \"$MESSAGE\" >> order.txt"]
run_windows = ["echo %MESSAGE%>>order.txt"]

[tasks.cleanup]
run = ["printf 'cleanup\\n' >> order.txt"]
run_windows = ["echo cleanup>>order.txt"]
"#,
    );

    let output = run(root.path(), &["run", "build", "--jobs", "2"]);
    assert_success(&output);
    let lines = fs::read_to_string(root.path().join("order.txt")).unwrap();
    let lines = lines.lines().map(str::trim).collect::<Vec<_>>();
    assert_eq!(lines, ["prepare", "hello", "cleanup"]);
}

#[test]
fn dry_run_json_never_starts_commands_or_writes_cache() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("input.txt"), "input").unwrap();
    write_plan(
        root.path(),
        r#"
schema = 2

[tasks.copy]
cache = true
sources = ["input.txt"]
outputs = ["output.txt"]
run = ["cat input.txt > output.txt"]
run_windows = ["type input.txt > output.txt"]
"#,
    );

    let output = run(root.path(), &["--json", "run", "copy", "--dry-run"]);
    assert_success(&output);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert!(!root.path().join("output.txt").exists());
    assert!(!root.path().join(".zed/task-cache").exists());
}

#[test]
fn content_cache_skips_exact_replays_and_reruns_after_source_drift() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("input.txt"), "one").unwrap();
    write_plan(
        root.path(),
        r#"
schema = 2

[tasks.copy]
cache = true
sources = ["input.txt"]
outputs = ["output.txt"]
run = ["cat input.txt > output.txt"]
run_windows = ["type input.txt > output.txt"]
"#,
    );

    let first = run(root.path(), &["run", "copy"]);
    assert_success(&first);
    assert_eq!(fs::read_to_string(root.path().join("output.txt")).unwrap(), "one");

    let second = run(root.path(), &["run", "copy"]);
    assert_success(&second);
    assert!(String::from_utf8_lossy(&second.stderr).contains("incremental cache hit"));

    fs::write(root.path().join("input.txt"), "two").unwrap();
    let third = run(root.path(), &["run", "copy"]);
    assert_success(&third);
    assert_eq!(fs::read_to_string(root.path().join("output.txt")).unwrap(), "two");
}

#[test]
fn confirmation_and_json_stream_boundaries_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    write_plan(
        root.path(),
        r#"
schema = 2

[tasks.release]
confirm = "publish release?"
run = ["echo release"]
"#,
    );

    let denied = run(root.path(), &["run", "release"]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("requires confirmation"));

    let json_live = run(root.path(), &["--json", "run", "release", "--yes"]);
    assert!(!json_live.status.success());
    assert!(String::from_utf8_lossy(&json_live.stderr).contains("requires `--dry-run`"));

    let approved = run(root.path(), &["run", "release", "--yes"]);
    assert_success(&approved);
}

#[test]
fn task_arguments_are_exposed_without_shell_interpolation() {
    let root = tempfile::tempdir().unwrap();
    write_plan(
        root.path(),
        r#"
schema = 2

[tasks.args]
run = ["printf '%s' \"$ZED_TASK_ARG_0\" > arg.txt"]
run_windows = ["echo %ZED_TASK_ARG_0%>arg.txt"]
"#,
    );

    let output = run(root.path(), &["run", "args", "--", "hello world"]);
    assert_success(&output);
    assert_eq!(
        fs::read_to_string(root.path().join("arg.txt"))
            .unwrap()
            .trim(),
        "hello world"
    );
}
