use std::{collections::BTreeSet, fs, path::PathBuf};

fn parse_toml(path: PathBuf) -> toml::Value {
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    toml::from_str(&text)
        .unwrap_or_else(|error| panic!("failed to parse {} as TOML: {error}", path.display()))
}

fn package_field(document: &toml::Value, field: &str) -> String {
    document
        .get("package")
        .and_then(|package| package.get(field))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("missing string package.{field}"))
        .to_owned()
}

fn cli_field<'a>(document: &'a toml::Value, field: &str) -> &'a str {
    document
        .get("cli")
        .and_then(|cli| cli.get(field))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("missing string cli.{field}"))
}

#[test]
fn zpkg_package_identity_matches_cargo_package_identity() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cargo = parse_toml(root.join("Cargo.toml"));
    let zpkg = parse_toml(root.join(".zpkg.toml"));

    assert_eq!(
        package_field(&zpkg, "name"),
        package_field(&cargo, "name"),
        ".zpkg.toml and Cargo.toml describe the same package and must not disagree on name"
    );
    assert_eq!(
        package_field(&zpkg, "version"),
        package_field(&cargo, "version"),
        ".zpkg.toml package.version must match Cargo.toml package.version so fleet dependency sync cannot derive stale or downgraded ranges"
    );
}

#[test]
fn zpkg_points_to_the_repository_owned_cli_contract() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let zpkg = parse_toml(root.join(".zpkg.toml"));

    assert_eq!(cli_field(&zpkg, "flags_contract"), ".cli-flags.toml");
    assert_eq!(cli_field(&zpkg, "flags_runtime"), "flags-2-env");
    assert_eq!(cli_field(&zpkg, "primary_bin"), "zed");
}

#[test]
fn every_public_zpkg_bin_survives_the_build_output_allowlist() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let zpkg = parse_toml(root.join(".zpkg.toml"));

    let outputs: BTreeSet<&str> = zpkg
        .get("build")
        .and_then(|build| build.get("outputs"))
        .and_then(toml::Value::as_array)
        .expect("missing build.outputs array")
        .iter()
        .map(|value| value.as_str().expect("build.outputs entries must be strings"))
        .collect();

    let bins = zpkg
        .get("bin")
        .and_then(toml::Value::as_table)
        .expect("missing bin table");
    let primary_bin = cli_field(&zpkg, "primary_bin");
    assert!(
        bins.contains_key(primary_bin),
        "cli.primary_bin `{primary_bin}` must be exposed by the Zed package"
    );

    for (name, value) in bins {
        let path = value
            .as_str()
            .unwrap_or_else(|| panic!("bin.{name} must be a string path"));
        assert!(
            outputs.contains(path),
            "public Zed bin `{name}` points to `{path}`, but that artifact is stripped by build.outputs"
        );
    }
}
