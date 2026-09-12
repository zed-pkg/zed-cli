use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn write_manifest(root: &Path) {
    fs::write(
        root.join(".zpkg.toml"),
        r#"
[package]
org = "acme"
name = "html-report"
version = "1.2.3"

[package.repository]
url = "https://github.com/acme/html-report"
"#,
    )
    .unwrap();
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(args)
        .current_dir(root)
        .env_remove("ZED_PKG_RELEASE_JSON")
        .env_remove("ZED_PKG_RELEASE_HTML")
        .output()
        .expect("run zed release plan")
}

#[test]
fn html_flag_writes_a_self_contained_report_without_changing_stdout_formats() {
    let root = tempfile::tempdir().unwrap();
    write_manifest(root.path());
    let report = root.path().join("reports/release.html");

    let output = run(
        root.path(),
        &["release", "plan", "--html", report.to_str().unwrap()],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(report.to_str().unwrap()));

    let html = fs::read_to_string(&report).unwrap();
    assert!(html.contains("acme/html-report@1.2.3#v1.2.3"));
    assert!(html.contains("default-src &#39;none&#39;") || html.contains("default-src 'none'"));
    assert!(!html.contains("src=\"http"));

    let human = run(root.path(), &["release", "plan"]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).starts_with("release set "));

    let json = run(root.path(), &["release", "plan", "--json"]);
    assert!(json.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(parsed["release_set"], "acme/html-report@1.2.3#v1.2.3");
}

#[test]
fn html_environment_fallback_and_conflict_handling_are_explicit() {
    let root = tempfile::tempdir().unwrap();
    write_manifest(root.path());
    let report = root.path().join("from-env.html");

    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(["release", "plan"])
        .current_dir(root.path())
        .env("ZED_PKG_RELEASE_HTML", &report)
        .env_remove("ZED_PKG_RELEASE_JSON")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.is_file());

    let conflict = run(
        root.path(),
        &[
            "release",
            "plan",
            "--json",
            "--html",
            report.to_str().unwrap(),
        ],
    );
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("cannot be used with"));
}

#[cfg(unix)]
#[test]
fn html_output_refuses_to_follow_an_existing_symbolic_link() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    write_manifest(root.path());
    let protected = root.path().join("protected.txt");
    let report = root.path().join("release.html");
    fs::write(&protected, "do not replace").unwrap();
    symlink(&protected, &report).unwrap();

    let output = run(
        root.path(),
        &["release", "plan", "--html", report.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("symbolic link"));
    assert_eq!(fs::read_to_string(protected).unwrap(), "do not replace");
}
