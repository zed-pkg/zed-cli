use std::{fs, path::PathBuf};

fn package_version(path: PathBuf) -> String {
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let document: toml::Value = toml::from_str(&text)
        .unwrap_or_else(|error| panic!("failed to parse {} as TOML: {error}", path.display()));

    document
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .unwrap_or_else(|| panic!("missing string package.version in {}", path.display()))
        .to_owned()
}

#[test]
fn zpkg_package_version_matches_cargo_package_version() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cargo_version = package_version(root.join("Cargo.toml"));
    let zpkg_version = package_version(root.join(".zpkg.toml"));

    assert_eq!(
        zpkg_version, cargo_version,
        ".zpkg.toml package.version must match Cargo.toml package.version so fleet dependency sync cannot derive stale or downgraded ranges"
    );
}
