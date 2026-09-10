use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use flags2env::BundledFlags2Env;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn every_root_toml_parses_and_cli_flag_contracts_pass_flags2env_audit() {
    let mut parsed = Vec::new();
    let parser = BundledFlags2Env::new();

    for entry in fs::read_dir(root()).expect("read repository root") {
        let entry = entry.expect("read root entry");
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .expect("UTF-8 root TOML name");
        let text = fs::read_to_string(&path).expect("read root TOML");
        toml::from_str::<toml::Value>(&text)
            .unwrap_or_else(|error| panic!("{name} is invalid TOML: {error}"));
        parsed.push(name.to_owned());

        if name.ends_with("cli-flags.toml") {
            let display = path.to_string_lossy();
            parser
                .audit_config(Some(display.as_ref()))
                .unwrap_or_else(|error| panic!("flags-2-env rejected {name}: {error}"));
        }
    }

    assert!(parsed.iter().any(|name| name == ".zpkg.toml"));
    assert!(parsed.iter().any(|name| name == ".cli-flags.toml"));
}

#[test]
fn repository_zpkg_manifest_validates_against_the_checked_in_interface_contract() {
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .current_dir(root())
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
        "self-manifest validation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("JSON validation report");
    assert_eq!(report["valid"], true);
    assert_eq!(report["manifest"]["package"], "zed-pkg/zed-cli");
}

#[test]
fn zpkg_version_and_explicit_binary_surface_match_cargo() {
    let manifest_text = fs::read_to_string(root().join(".zpkg.toml")).expect("read zpkg manifest");
    let manifest: toml::Value = toml::from_str(&manifest_text).expect("parse zpkg manifest");

    assert_eq!(
        manifest["package"]["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        ".zpkg.toml package.version must track Cargo package version"
    );

    let expected = BTreeSet::from([
        "zed".to_owned(),
        "zed-binary".to_owned(),
        "zed-git-install".to_owned(),
        "zed-gitops".to_owned(),
    ]);
    let bins = manifest["bin"]
        .as_table()
        .expect("zpkg [bin] table")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    assert_eq!(bins, expected, ".zpkg.toml must retain every explicit Cargo binary");

    let outputs = manifest["build"]["outputs"]
        .as_array()
        .expect("zpkg build.outputs")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("string build output")
                .strip_prefix("target/release/")
                .expect("release binary output")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(outputs, expected, "zpkg build outputs must match explicit Cargo binaries");

    assert!(
        manifest.get("cli").is_none(),
        "legacy non-schema [cli] metadata must not reappear"
    );
}
