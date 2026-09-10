use std::{fs, process::Command};

use flags2env::BundledFlags2Env;

const CHILD_PROBE: &str = "ZED_TEST_DOTENV_CHILD_PROBE";
const SENTINEL_REGISTRY: &str = "https://dotenv-must-not-load.invalid";

#[test]
fn working_directory_dotenv_is_ignored_by_primary_flags_contract() {
    if std::env::var_os(CHILD_PROBE).is_some() {
        let contract = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cli-flags.toml");
        let contract = contract
            .to_str()
            .expect("repository path must be valid UTF-8 for flags2env");
        let argv = vec!["zed".to_string(), "install".to_string()];
        let parsed = BundledFlags2Env::new()
            .parse_structured(&argv, Some(contract))
            .expect("isolated flags contract must parse");

        assert_eq!(
            parsed.flags.get("ZED_PKG_REGISTRY").map(String::as_str),
            Some("https://registry.zpkg.net"),
            "working-directory .env must not override the declared registry default"
        );
        assert!(
            !parsed.flags.contains_key("ZED_PKG_TOKEN"),
            "working-directory .env must not inject a registry credential into parser output"
        );
        return;
    }

    let workdir = tempfile::tempdir().expect("create isolated dotenv probe directory");
    fs::write(
        workdir.path().join(".env"),
        format!(
            "ZED_PKG_REGISTRY={SENTINEL_REGISTRY}\nZED_PKG_TOKEN=dotenv-secret-sentinel\n"
        ),
    )
    .expect("write hostile working-directory .env fixture");

    let output = Command::new(std::env::current_exe().expect("locate integration-test executable"))
        .arg("--exact")
        .arg("working_directory_dotenv_is_ignored_by_primary_flags_contract")
        .arg("--nocapture")
        .env(CHILD_PROBE, "1")
        .current_dir(workdir.path())
        .output()
        .expect("run isolated child parser probe");

    assert!(
        output.status.success(),
        "isolated dotenv child probe failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
