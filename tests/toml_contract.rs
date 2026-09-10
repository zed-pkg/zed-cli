use std::fs;
use std::path::Path;

fn parse_toml(path: &str) -> toml::Value {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("failed to read {path}: {error}"));
    toml::from_str(&source).unwrap_or_else(|error| panic!("invalid TOML in {path}: {error}"))
}

fn string_at<'a>(document: &'a toml::Value, path: &[&str]) -> &'a str {
    let mut value = document;
    for segment in path {
        value = value
            .get(*segment)
            .unwrap_or_else(|| panic!("missing TOML key {}", path.join(".")));
    }
    value
        .as_str()
        .unwrap_or_else(|| panic!("TOML key {} must be a string", path.join(".")))
}

#[test]
fn cargo_and_zpkg_package_versions_are_identical() {
    let cargo = parse_toml("Cargo.toml");
    let zpkg = parse_toml(".zpkg.toml");

    assert_eq!(string_at(&cargo, &["package", "name"]), "zed-cli");
    assert_eq!(string_at(&zpkg, &["package", "name"]), "zed-cli");
    assert_eq!(
        string_at(&cargo, &["package", "version"]),
        string_at(&zpkg, &["package", "version"]),
        "Cargo.toml and .zpkg.toml are release authorities for the same CLI and must not drift",
    );
}

#[test]
fn zpkg_points_to_the_repository_owned_flags2env_contract() {
    let zpkg = parse_toml(".zpkg.toml");
    assert_eq!(
        string_at(&zpkg, &["cli", "flags_contract"]),
        ".cli-flags.toml"
    );
    assert_eq!(
        string_at(&zpkg, &["cli", "flags_runtime"]),
        "flags-2-env"
    );
    assert_eq!(string_at(&zpkg, &["cli", "primary_bin"]), "zed");
}

#[test]
fn all_checked_in_cli_flag_contracts_are_valid_toml() {
    for path in [
        ".cli-flags.toml",
        ".dev-cli-flags.toml",
        ".fetch-cli-flags.toml",
    ] {
        let document = parse_toml(path);
        assert!(document.is_table(), "{path} must have a TOML table root");
    }

    let flags = parse_toml(".cli-flags.toml");
    assert_eq!(
        flags
            .get("parse")
            .and_then(|value| value.get("allow_unknown"))
            .and_then(toml::Value::as_bool),
        Some(false),
        "the canonical CLI contract must fail closed on unknown options",
    );
    assert_eq!(
        string_at(&flags, &["help", "url"]),
        "https://github.com/zed-pkg/zed-cli"
    );
}
