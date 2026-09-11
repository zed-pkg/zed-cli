use std::path::Path;
use std::process::Command;

#[test]
fn repository_zpkg_manifest_is_accepted_by_built_validator() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command
        .current_dir(root)
        .args([
            "validate",
            "--manifest",
            ".zpkg.toml",
            "--lock",
            ".zpkg.lock",
            "--json",
        ])
        .env_remove("ZED_PKG_HOME")
        .env_remove("ZED_PKG_TOKEN")
        .env_remove("ZED_PKG_VALIDATE_MANIFEST")
        .env_remove("ZED_PKG_VALIDATE_LOCK")
        .env_remove("ZED_PKG_VALIDATE_REQUIRE_LOCK")
        .env_remove("ZED_PKG_VALIDATE_JSON")
        .env_remove("ZED_PKG_COMMAND")
        .env_remove("ZED_PKG_PARSE_ERRORS")
        .env_remove("ZED_PKG_UNKNOWN_OPTIONS");

    let output = command.output().expect("run built zed validator");
    assert!(
        output.status.success(),
        "repository .zpkg.toml must validate with the built CLI; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "validator emitted invalid JSON: {error}; stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(report["valid"], true);
    assert_eq!(report["manifest"]["package"], "zed-pkg/zed-cli");

    // This repository currently has direct Zed dependencies but intentionally
    // owns no .zpkg.lock. Keep that limitation explicit so schema self-checking
    // cannot be mistaken for frozen dependency verification.
    assert!(report["manifest"]["direct_requirements"]
        .as_u64()
        .is_some_and(|count| count > 0));
    assert_eq!(report["lock"]["present"], false);
    assert_eq!(report["direct_requirements_checked"], 0);
    assert!(report["warnings"]
        .as_array()
        .is_some_and(|warnings| !warnings.is_empty()));
}
