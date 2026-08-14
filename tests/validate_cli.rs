use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const VALID: &str = "tests/fixtures/validate/valid";
const SUBMODULE: &str = "tests/fixtures/validate/git-submodule";
const INTERFACE_REVISION: &str = "5163b661a2b91120701fe4a65c43586addb70868";

fn zed() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_zed"))
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(name)
}

fn project(name: &str) -> tempfile::TempDir {
    let source = fixture(name);
    let project = tempfile::tempdir().unwrap();
    for descriptor in [".zpkg.toml", ".zpkg.lock"] {
        fs::copy(source.join(descriptor), project.path().join(descriptor)).unwrap();
    }
    project
}

fn command(root: &Path) -> Command {
    let mut command = Command::new(zed());
    command.current_dir(root);
    for key in [
        "ZED_PKG_HOME",
        "ZED_PKG_TOKEN",
        "ZED_PKG_INTERACTIVE",
        "ZED_PKG_GIT_SUBMODULES",
        "ZED_PKG_VALIDATE_MANIFEST",
        "ZED_PKG_VALIDATE_LOCK",
        "ZED_PKG_VALIDATE_REQUIRE_LOCK",
        "ZED_PKG_VALIDATE_JSON",
    ] {
        command.env_remove(key);
    }
    command.env("ZED_PKG_HOME", root.join("home-must-not-be-created"));
    command
}

fn run(root: &Path, args: &[&str]) -> Output {
    command(root).args(args).output().expect("run zed validate")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_failure_contains(root: &Path, expected: &str) {
    let output = run(root, &["validate", "--require-lock"]);
    assert!(!output.status.success(), "unexpected success");
    let error = stderr(&output);
    assert!(
        error.contains(expected),
        "expected {expected:?} in:\n{error}"
    );
}

#[test]
fn valid_pair_has_deterministic_json_and_never_mutates_project_or_home() {
    let project = project(VALID);
    let manifest_before = fs::read(project.path().join(".zpkg.toml")).unwrap();
    let lock_before = fs::read(project.path().join(".zpkg.lock")).unwrap();
    let staging = project.path().join(zed_cli::transaction::STAGING_DIR);
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("sentinel"), "must survive").unwrap();

    let first = run(project.path(), &["validate", "--require-lock", "--json"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let second = run(project.path(), &["validate", "--require-lock", "--json"]);
    assert!(second.status.success(), "{}", stderr(&second));
    assert_eq!(first.stdout, second.stdout);

    let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(report["valid"], true);
    assert_eq!(report["interface_revision"], INTERFACE_REVISION);
    assert_eq!(report["manifest"]["package"], "acme/validator-fixture");
    assert_eq!(report["lock"]["version"], 1);
    assert_eq!(report["direct_requirements_checked"], 2);
    assert_eq!(
        report["transitive_completeness"],
        "not-verifiable-in-lockfile-v1-without-dependency-edges"
    );

    assert_eq!(
        fs::read(project.path().join(".zpkg.toml")).unwrap(),
        manifest_before
    );
    assert_eq!(
        fs::read(project.path().join(".zpkg.lock")).unwrap(),
        lock_before
    );
    assert_eq!(
        fs::read_to_string(staging.join("sentinel")).unwrap(),
        "must survive"
    );
    assert!(!project.path().join("home-must-not-be-created").exists());
}

#[test]
fn documented_language_aliases_and_legacy_polyglot_marker_validate() {
    for alias in ["javascript", "typescript", "js", "ts", "go", "polyglot"] {
        let project = project(VALID);
        let text = fs::read_to_string(project.path().join(".zpkg.toml"))
            .unwrap()
            .replacen(
                "version = \"1.0.0\"",
                &format!("version = \"1.0.0\"\nlanguage = \"{alias}\""),
                1,
            );
        fs::write(project.path().join(".zpkg.toml"), text).unwrap();

        let output = run(project.path(), &["validate", "--require-lock", "--json"]);
        assert!(
            output.status.success(),
            "alias {alias:?}: {}",
            stderr(&output)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["valid"], true);
        assert_eq!(report["interface_revision"], INTERFACE_REVISION);
    }

    let unknown = project(VALID);
    let text = fs::read_to_string(unknown.path().join(".zpkg.toml"))
        .unwrap()
        .replacen(
            "version = \"1.0.0\"",
            "version = \"1.0.0\"\nlanguage = \"brainfuck\"",
            1,
        );
    fs::write(unknown.path().join(".zpkg.toml"), text).unwrap();
    assert_failure_contains(unknown.path(), "unknown language `brainfuck`");
}

#[test]
fn explicit_paths_accept_the_validated_git_submodule_extension() {
    let project = project(SUBMODULE);
    let output = run(
        project.path(),
        &[
            "validate",
            "--manifest",
            ".zpkg.toml",
            "--lock=.zpkg.lock",
            "--require-lock",
            "--json",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["lock"]["git_submodules"], 1);
    assert_eq!(report["direct_requirements_checked"], 1);
}

#[test]
fn malformed_toml_unknown_fields_and_bad_extension_fields_fail_closed() {
    let malformed = project(VALID);
    fs::write(malformed.path().join(".zpkg.toml"), "[package\n").unwrap();
    assert_failure_contains(malformed.path(), "parsing manifest as TOML");

    let unknown_manifest = project(VALID);
    let text = fs::read_to_string(unknown_manifest.path().join(".zpkg.toml"))
        .unwrap()
        .replacen(
            "version = \"1.0.0\"",
            "version = \"1.0.0\"\nsurprise = true",
            1,
        );
    fs::write(unknown_manifest.path().join(".zpkg.toml"), text).unwrap();
    assert_failure_contains(unknown_manifest.path(), "$.package.surprise");

    let unknown_lock = project(VALID);
    let text = fs::read_to_string(unknown_lock.path().join(".zpkg.lock"))
        .unwrap()
        .replacen("version = 1", "version = 1\nfuture = true", 1);
    fs::write(unknown_lock.path().join(".zpkg.lock"), text).unwrap();
    assert_failure_contains(unknown_lock.path(), "$.future");

    let unknown_extension = project(SUBMODULE);
    let mut text = fs::read_to_string(unknown_extension.path().join(".zpkg.lock")).unwrap();
    text.push_str("unknown = true\n");
    fs::write(unknown_extension.path().join(".zpkg.lock"), text).unwrap();
    assert_failure_contains(unknown_extension.path(), "unknown field");
}

#[test]
fn lock_version_slug_and_provenance_are_runtime_validated() {
    let version = project(VALID);
    let text = fs::read_to_string(version.path().join(".zpkg.lock"))
        .unwrap()
        .replacen("version = 1", "version = 2", 1);
    fs::write(version.path().join(".zpkg.lock"), text).unwrap();
    assert_failure_contains(version.path(), "unsupported lockfile version 2");

    let slug = project(VALID);
    let text = fs::read_to_string(slug.path().join(".zpkg.lock"))
        .unwrap()
        .replacen("name = \"runtime\"", "name = \"Bad_Name\"", 1);
    fs::write(slug.path().join(".zpkg.lock"), text).unwrap();
    assert_failure_contains(slug.path(), "name must be a lowercase slug");

    let provenance = project(VALID);
    let text = fs::read_to_string(provenance.path().join(".zpkg.lock"))
        .unwrap()
        .replacen(
            "vcs_commit = \"1111111111111111111111111111111111111111\"",
            "vcs_commit = \"main\"",
            1,
        );
    fs::write(provenance.path().join(".zpkg.lock"), text).unwrap();
    assert_failure_contains(provenance.path(), "vcs_commit must be");

    let extension_slug = project(SUBMODULE);
    let text = fs::read_to_string(extension_slug.path().join(".zpkg.lock"))
        .unwrap()
        .replacen(
            "package = \"acme/subtool\"",
            "package = \"ACME/subtool\"",
            1,
        );
    fs::write(extension_slug.path().join(".zpkg.lock"), text).unwrap();
    assert_failure_contains(extension_slug.path(), "non-canonical Git submodule package");
}

#[test]
fn direct_requirement_drift_and_incomplete_lock_state_fail() {
    let drift = project(VALID);
    let text = fs::read_to_string(drift.path().join(".zpkg.lock"))
        .unwrap()
        .replacen("version = \"1.9.0\"", "version = \"2.1.0\"", 1);
    fs::write(drift.path().join(".zpkg.lock"), text).unwrap();
    assert_failure_contains(drift.path(), "requires `^1.2`");

    let incomplete = project(VALID);
    fs::write(incomplete.path().join(".zpkg.lock"), "version = 1\n").unwrap();
    assert_failure_contains(incomplete.path(), "incomplete lock state");
}

#[test]
fn lock_is_optional_by_default_but_require_lock_returns_json_failure() {
    let project = project(VALID);
    fs::remove_file(project.path().join(".zpkg.lock")).unwrap();

    let optional = run(project.path(), &["validate", "--json"]);
    assert!(optional.status.success(), "{}", stderr(&optional));
    let report: serde_json::Value = serde_json::from_slice(&optional.stdout).unwrap();
    assert_eq!(report["valid"], true);
    assert_eq!(report["lock"]["present"], false);

    let required = run(project.path(), &["validate", "--require-lock", "--json"]);
    assert!(!required.status.success());
    let failure: serde_json::Value = serde_json::from_slice(&required.stdout).unwrap();
    assert_eq!(failure["valid"], false);
    assert!(
        failure["error"]
            .as_str()
            .unwrap()
            .contains("required lockfile")
    );
}
