use std::fs;

use flags2env::BundledFlags2Env;
use walkdir::WalkDir;

const CLI_CONTRACTS: &[&str] = &[
    ".cli-flags.toml",
    ".dev-cli-flags.toml",
    ".fetch-cli-flags.toml",
    ".nix-interop-cli-flags.toml",
    ".task-cli-flags.toml",
    ".tool-cli-flags.toml",
];

#[test]
fn every_cli_contract_passes_flags2env_audit() {
    let parser = BundledFlags2Env::new();
    for path in CLI_CONTRACTS {
        parser
            .audit_config(Some(path))
            .unwrap_or_else(|error| panic!("flags2env audit failed for {path}: {error}"));
    }
}

#[test]
fn every_repository_toml_file_parses() {
    let mut checked = 0usize;
    let mut errors = Vec::new();

    for entry in WalkDir::new(".").into_iter().filter_entry(|entry| {
        !matches!(entry.file_name().to_str(), Some(".git" | "target" | ".zed"))
    }) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!("walk error: {error}"));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !(name.ends_with(".toml") || name.ends_with(".toml.example")) {
            continue;
        }

        checked += 1;
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        if let Err(error) = toml::from_str::<toml::Value>(&content) {
            errors.push(format!("{}: {error}", path.display()));
        }
    }

    assert!(
        checked >= CLI_CONTRACTS.len(),
        "expected at least the six CLI contracts, parsed only {checked} TOML files"
    );
    assert!(
        errors.is_empty(),
        "invalid repository TOML:\n{}",
        errors.join("\n")
    );
}

#[test]
fn zpkg_manifest_matches_cargo_package_identity() {
    let zpkg = fs::read_to_string(".zpkg.toml").expect("read .zpkg.toml");
    let zpkg: toml::Value = toml::from_str(&zpkg).expect("parse .zpkg.toml");
    let cargo = fs::read_to_string("Cargo.toml").expect("read Cargo.toml");
    let cargo: toml::Value = toml::from_str(&cargo).expect("parse Cargo.toml");

    for field in ["name", "version"] {
        assert_eq!(
            zpkg["package"][field].as_str(),
            cargo["package"][field].as_str(),
            ".zpkg.toml package.{field} must match Cargo.toml package.{field}"
        );
    }
}

#[test]
fn zpkg_manifest_keeps_the_repository_owned_cli_contract() {
    let zpkg = fs::read_to_string(".zpkg.toml").expect("read .zpkg.toml");
    let zpkg: toml::Value = toml::from_str(&zpkg).expect("parse .zpkg.toml");

    assert_eq!(
        zpkg["cli"]["flags_contract"].as_str(),
        Some(".cli-flags.toml")
    );
    assert_eq!(
        zpkg["cli"]["flags_runtime"].as_str(),
        Some("flags-2-env")
    );
    assert_eq!(zpkg["cli"]["primary_bin"].as_str(), Some("zed"));
}

#[test]
fn every_cli_contract_remains_fail_closed() {
    for path in CLI_CONTRACTS {
        let contract = fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"));
        let contract: toml::Value = toml::from_str(&contract)
            .unwrap_or_else(|error| panic!("parse {path}: {error}"));
        assert_eq!(
            contract["parse"]["allow_unknown"].as_bool(),
            Some(false),
            "{path} must reject unknown options"
        );
    }

    let canonical = fs::read_to_string(".cli-flags.toml").expect("read .cli-flags.toml");
    let canonical: toml::Value = toml::from_str(&canonical).expect("parse .cli-flags.toml");
    assert_eq!(
        canonical["help"]["url"].as_str(),
        Some("https://github.com/zed-pkg/zed-cli")
    );
}

#[test]
fn manifestless_environment_migration_remains_explicit() {
    let contract = fs::read_to_string(".cli-flags.toml").expect("read .cli-flags.toml");
    let contract: toml::Value = toml::from_str(&contract).expect("parse .cli-flags.toml");
    let flag = &contract["flags"]["do_not_write_new_manifest"];

    assert_eq!(
        flag["env"].as_str(),
        Some(zed_cli::cli_model::LEGACY_ALLOW_NO_MANIFEST_ENV),
        "the embedded flags2env contract intentionally consumes the legacy compatibility key"
    );

    let aliases = flag["aliases"]
        .as_array()
        .expect("manifestless flag aliases")
        .iter()
        .filter_map(toml::Value::as_str)
        .collect::<Vec<_>>();
    for spelling in [
        "do-not-write-new-manifest",
        "allow-no-manifest",
        "skip-manifest",
    ] {
        assert!(
            aliases.contains(&spelling),
            "missing CLI spelling {spelling}"
        );
    }

    assert_eq!(
        zed_cli::cli_model::DO_NOT_WRITE_NEW_MANIFEST_ENV,
        "ZED_PKG_DO_NOT_WRITE_NEW_MANIFEST",
        "the public canonical environment key must not regress to the compatibility key"
    );
}
