use std::{fs, path::Path};

const CANONICAL_FLAG_TYPES: &[&str] = &[
    "array", "bool", "double", "integer", "json", "map", "string",
];

fn assert_canonical_types(path: &Path, value: &toml::Value, location: &str) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let child_location = if location.is_empty() {
                    key.clone()
                } else {
                    format!("{location}.{key}")
                };

                if key == "type" {
                    let kind = child.as_str().unwrap_or_else(|| {
                        panic!(
                            "{}: {child_location} must be a string flag type",
                            path.display()
                        )
                    });
                    assert!(
                        CANONICAL_FLAG_TYPES.contains(&kind),
                        "{}: {child_location} uses noncanonical flags2env type `{kind}`; expected one of {}",
                        path.display(),
                        CANONICAL_FLAG_TYPES.join(", ")
                    );
                }

                assert_canonical_types(path, child, &child_location);
            }
        }
        toml::Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                assert_canonical_types(path, child, &format!("{location}[{index}]"));
            }
        }
        _ => {}
    }
}

#[test]
fn cli_flags_toml_uses_canonical_flags2env_type_names() {
    let mut contracts = fs::read_dir(".")
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
    contracts.sort();

    assert!(
        contracts.len() >= 6,
        "expected canonical and auxiliary CLI flags contracts"
    );

    for path in contracts {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let value = toml::from_str::<toml::Value>(&source)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
        assert_canonical_types(&path, &value, "");
    }
}
