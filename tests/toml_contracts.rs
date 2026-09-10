use std::{collections::BTreeSet, fs, path::PathBuf};

use flags2env::BundledFlags2Env;
use walkdir::WalkDir;

fn read_toml(path: &str) -> toml::Value {
    let source = fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    toml::from_str(&source).unwrap_or_else(|error| panic!("parse {path}: {error}"))
}

fn cli_contract_paths() -> Vec<PathBuf> {
    let mut paths = fs::read_dir(".")
        .expect("read repository root")
        .map(|entry| entry.expect("read repository-root entry").path())
        .filter(|path| {
            if !path.is_file() {
                return false;
            }
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name == ".cli-flags.toml"
                        || (name.starts_with('.') && name.ends_with("-cli-flags.toml"))
                })
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[test]
fn every_cli_contract_passes_flags2env_audit() {
    let paths = cli_contract_paths();
    assert!(
        paths.iter().any(|path| {
            path.file_name().and_then(|name| name.to_str()) == Some(".cli-flags.toml")
        }),
        "repository root must contain .cli-flags.toml"
    );
    assert!(
        paths.len() >= 6,
        "expected the canonical contract plus the checked-in auxiliary contracts"
    );

    let parser = BundledFlags2Env::new();
    for path in paths {
        let display = path.display().to_string();
        let path = path
            .to_str()
            .unwrap_or_else(|| panic!("CLI contract path is not UTF-8: {display}"));
        parser
            .audit_config(Some(path))
            .unwrap_or_else(|error| panic!("flags2env audit failed for {display}: {error}"));
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
        checked >= cli_contract_paths().len(),
        "expected to parse at least every CLI contract, parsed only {checked} TOML files"
    );
    assert!(
        errors.is_empty(),
        "invalid repository TOML:\n{}",
        errors.join("\n")
    );
}

#[test]
fn zpkg_manifest_matches_cargo_package_identity() {
    let zpkg = read_toml(".zpkg.toml");
    let cargo = read_toml("Cargo.toml");

    for field in ["name", "version"] {
        assert_eq!(
            zpkg["package"][field].as_str(),
            cargo["package"][field].as_str(),
            ".zpkg.toml package.{field} must match Cargo.toml package.{field}"
        );
    }
}

#[test]
fn zpkg_manifest_covers_every_cargo_binary_and_build_output() {
    let zpkg = read_toml(".zpkg.toml");
    let cargo = read_toml("Cargo.toml");

    let cargo_bins = cargo["bin"]
        .as_array()
        .expect("Cargo.toml [[bin]] entries")
        .iter()
        .map(|entry| {
            entry["name"]
                .as_str()
                .expect("Cargo.toml [[bin]].name")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    let zpkg_bins = zpkg["bin"]
        .as_table()
        .expect(".zpkg.toml [bin] table")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();

    assert_eq!(
        &zpkg_bins, &cargo_bins,
        ".zpkg.toml [bin] names must match Cargo.toml [[bin]] names"
    );

    let outputs = zpkg["build"]["outputs"]
        .as_array()
        .expect(".zpkg.toml build.outputs")
        .iter()
        .map(|output| {
            output
                .as_str()
                .expect(".zpkg.toml build output must be a string")
                .to_owned()
        })
        .collect::<BTreeSet<_>>();
    let primary_bin = zpkg["cli"]["primary_bin"]
        .as_str()
        .expect(".zpkg.toml cli.primary_bin");

    assert!(
        zpkg_bins.contains(primary_bin),
        ".zpkg.toml cli.primary_bin `{primary_bin}` must be exposed by [bin]"
    );

    for name in cargo_bins {
        let expected = format!("target/release/{name}");
        assert_eq!(
            zpkg["bin"][name.as_str()].as_str(),
            Some(expected.as_str()),
            ".zpkg.toml [bin].{name} must install the Cargo release binary"
        );
        assert!(
            outputs.contains(&expected),
            ".zpkg.toml build.outputs must include {expected}"
        );
    }
}

#[test]
fn zpkg_manifest_keeps_the_repository_owned_cli_contract() {
    let zpkg = read_toml(".zpkg.toml");

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
fn flags2env_dependency_is_canonical_and_immutable() {
    let cargo = read_toml("Cargo.toml");
    let dependency = cargo["dependencies"]["flags2env"]
        .as_table()
        .expect("flags2env Git dependency");

    assert_eq!(
        dependency.get("git").and_then(toml::Value::as_str),
        Some("https://github.com/flags-2-env/flags-2-env.git")
    );
    let revision = dependency
        .get("rev")
        .and_then(toml::Value::as_str)
        .expect("flags2env immutable Git revision");
    assert_eq!(revision.len(), 40, "flags2env revision must be a full SHA-1");
    assert!(
        revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "flags2env revision must be a lowercase hexadecimal full SHA-1"
    );
}

#[test]
fn every_cli_contract_remains_fail_closed() {
    for path in cli_contract_paths() {
        let display = path.display().to_string();
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {display}: {error}"));
        let contract: toml::Value = toml::from_str(&source)
            .unwrap_or_else(|error| panic!("parse {display}: {error}"));
        assert_eq!(
            contract["parse"]["allow_unknown"].as_bool(),
            Some(false),
            "{display} must reject unknown options"
        );
    }

    let canonical = read_toml(".cli-flags.toml");
    assert_eq!(
        canonical["help"]["url"].as_str(),
        Some("https://github.com/zed-pkg/zed-cli")
    );
    for required in ["no_mirrors", "trust_mirror_metadata"] {
        assert!(
            canonical["flags"].get(required).is_some(),
            ".cli-flags.toml must retain required global flag {required}"
        );
    }
}

#[test]
fn manifestless_environment_migration_remains_explicit() {
    let contract = read_toml(".cli-flags.toml");
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
