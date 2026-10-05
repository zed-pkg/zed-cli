use std::{path::Path, process::Command};

#[test]
fn repository_manifest_is_accepted_by_the_built_zed_binary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .current_dir(root)
        .args([
            "validate",
            "--manifest",
            ".zpkg.toml",
            "--lock",
            ".zpkg.lock",
            "--json",
        ])
        .output()
        .expect("run zed self-validation");

    assert!(
        output.status.success(),
        "zed rejected its checked-in package manifest: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("zed validate must emit JSON in --json mode");
    assert_eq!(
        report.get("valid").and_then(serde_json::Value::as_bool),
        Some(true),
        "zed must report its checked-in .zpkg.toml/.zpkg.lock pair as valid"
    );
}
