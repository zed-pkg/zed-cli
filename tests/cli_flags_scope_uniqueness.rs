use std::{collections::BTreeSet, fs, path::Path};

use toml::Value;

fn public_spelling(name: &str) -> String {
    name.replace('_', "-")
}

fn validate_flags_table(path: &Path, scope: &str, flags: &toml::map::Map<String, Value>) {
    let mut environments = BTreeSet::new();
    let mut spellings = BTreeSet::new();

    for (name, value) in flags {
        let definition = value.as_table().unwrap_or_else(|| {
            panic!(
                "{}: {scope}.flags.{name} must be a table",
                path.display()
            )
        });
        let environment = definition
            .get("env")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!(
                    "{}: {scope}.flags.{name}.env must be a string",
                    path.display()
                )
            });
        assert!(
            !environment.is_empty(),
            "{}: {scope}.flags.{name}.env must not be empty",
            path.display()
        );
        assert!(
            environments.insert(environment.to_owned()),
            "{}: {scope} reuses environment key {environment}",
            path.display()
        );

        let canonical = public_spelling(name);
        assert!(
            spellings.insert(canonical.clone()),
            "{}: {scope} reuses public option spelling --{canonical}",
            path.display()
        );

        if let Some(aliases) = definition.get("aliases") {
            let aliases = aliases.as_array().unwrap_or_else(|| {
                panic!(
                    "{}: {scope}.flags.{name}.aliases must be an array",
                    path.display()
                )
            });
            for alias in aliases {
                let alias = alias.as_str().unwrap_or_else(|| {
                    panic!(
                        "{}: {scope}.flags.{name}.aliases must contain strings",
                        path.display()
                    )
                });
                assert!(
                    !alias.is_empty(),
                    "{}: {scope}.flags.{name} contains an empty alias",
                    path.display()
                );
                if alias == canonical {
                    continue;
                }
                assert!(
                    spellings.insert(alias.to_owned()),
                    "{}: {scope} reuses public option spelling --{alias}",
                    path.display()
                );
            }
        }
    }
}

fn visit_scopes(path: &Path, value: &Value, location: &str) {
    match value {
        Value::Table(table) => {
            if let Some(flags) = table.get("flags") {
                let flags = flags.as_table().unwrap_or_else(|| {
                    panic!("{}: {location}.flags must be a table", path.display())
                });
                validate_flags_table(path, location, flags);
            }
            for (name, child) in table {
                let child_location = if location.is_empty() {
                    name.clone()
                } else {
                    format!("{location}.{name}")
                };
                visit_scopes(path, child, &child_location);
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                visit_scopes(path, child, &format!("{location}[{index}]"));
            }
        }
        _ => {}
    }
}

fn cli_contract_paths() -> Vec<std::path::PathBuf> {
    let mut paths = fs::read_dir(".")
        .expect("read repository root")
        .map(|entry| entry.expect("read repository-root entry").path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
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
fn every_cli_scope_has_unique_environment_keys_and_public_spellings() {
    let paths = cli_contract_paths();
    assert!(
        paths.len() >= 6,
        "expected the canonical CLI contract and its auxiliary contracts"
    );

    for path in paths {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let contract: Value = toml::from_str(&source)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        visit_scopes(&path, &contract, "root");
    }
}
