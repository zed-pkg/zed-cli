use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn parse_toml(path: &Path) -> toml::Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    toml::from_str(&text).unwrap_or_else(|error| panic!("invalid TOML {}: {error}", path.display()))
}

fn string_at<'a>(value: &'a toml::Value, path: &[&str]) -> &'a str {
    let mut current = value;
    for segment in path {
        current = current
            .get(*segment)
            .unwrap_or_else(|| panic!("missing TOML path {}", path.join(".")));
    }
    current
        .as_str()
        .unwrap_or_else(|| panic!("TOML path {} must be a string", path.join(".")))
}

#[test]
fn every_repository_root_toml_file_parses() {
    let root = root();
    let mut files = fs::read_dir(&root)
        .expect("read repository root")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "toml"))
        .collect::<Vec<_>>();
    files.sort();

    assert!(files.iter().any(|path| path.ends_with("Cargo.toml")));
    assert!(files.iter().any(|path| path.ends_with(".zpkg.toml")));
    assert!(files.iter().any(|path| path.ends_with(".cli-flags.toml")));

    for path in files {
        let _ = parse_toml(&path);
    }
}

#[test]
fn zed_package_version_matches_the_native_release_version() {
    let root = root();
    let zpkg = parse_toml(&root.join(".zpkg.toml"));
    let cargo = parse_toml(&root.join("Cargo.toml"));

    let zpkg_version = string_at(&zpkg, &["package", "version"]);
    let cargo_version = string_at(&cargo, &["package", "version"]);
    assert_eq!(zpkg_version, cargo_version);
    assert_eq!(zpkg_version, env!("CARGO_PKG_VERSION"));
}

#[test]
fn zed_package_declares_the_canonical_flags_runtime_and_contract() {
    let root = root();
    let zpkg = parse_toml(&root.join(".zpkg.toml"));
    let cargo = parse_toml(&root.join("Cargo.toml"));
    let flags = parse_toml(&root.join(".cli-flags.toml"));

    assert_eq!(
        string_at(&zpkg, &["cli", "flags_contract"]),
        ".cli-flags.toml"
    );
    assert_eq!(string_at(&zpkg, &["cli", "flags_runtime"]), "flags-2-env");
    assert_eq!(string_at(&zpkg, &["cli", "primary_bin"]), "zed");

    let flags2env = cargo
        .get("dependencies")
        .and_then(|value| value.get("flags2env"))
        .and_then(toml::Value::as_table)
        .expect("Cargo.toml must declare flags2env as a structured dependency");
    assert_eq!(
        flags2env.get("git").and_then(toml::Value::as_str),
        Some("https://github.com/flags-2-env/flags-2-env.git")
    );
    let revision = flags2env
        .get("rev")
        .and_then(toml::Value::as_str)
        .expect("flags2env must use an immutable Git revision");
    assert_eq!(revision.len(), 40);
    assert!(revision.bytes().all(|byte| byte.is_ascii_hexdigit()));

    assert_eq!(
        flags
            .get("parse")
            .and_then(|value| value.get("allow_unknown"))
            .and_then(toml::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        string_at(&flags, &["help", "url"]),
        "https://github.com/zed-pkg/zed-cli"
    );
}

#[test]
fn global_flags_do_not_reuse_environment_keys_or_public_aliases() {
    let flags = parse_toml(&root().join(".cli-flags.toml"));
    let table = flags
        .get("flags")
        .and_then(toml::Value::as_table)
        .expect(".cli-flags.toml must declare [flags]");

    let mut envs = BTreeSet::new();
    let mut aliases = BTreeSet::new();
    for (canonical, definition) in table {
        let definition = definition
            .as_table()
            .unwrap_or_else(|| panic!("flags.{canonical} must be a table"));
        let env = definition
            .get("env")
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("flags.{canonical} must declare env"));
        assert!(envs.insert(env), "duplicate global flag env key: {env}");

        let canonical_alias = canonical.replace('_', "-");
        assert!(
            aliases.insert(canonical_alias.clone()),
            "duplicate global canonical flag alias: {canonical_alias}"
        );
        if let Some(values) = definition.get("aliases").and_then(toml::Value::as_array) {
            for value in values {
                let alias = value
                    .as_str()
                    .unwrap_or_else(|| panic!("flags.{canonical}.aliases must contain strings"));
                if alias == canonical_alias {
                    continue;
                }
                assert!(
                    aliases.insert(alias.to_owned()),
                    "duplicate global flag alias: {alias}"
                );
            }
        }
    }
}
