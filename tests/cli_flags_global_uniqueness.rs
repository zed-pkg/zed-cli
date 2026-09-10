use std::{collections::BTreeMap, fs};

#[test]
fn root_global_flags_do_not_reuse_environment_keys_or_public_spellings() {
    let source = fs::read_to_string(".cli-flags.toml").expect("read .cli-flags.toml");
    let contract: toml::Value = toml::from_str(&source).expect("parse .cli-flags.toml");
    let flags = contract["flags"]
        .as_table()
        .expect(".cli-flags.toml must declare [flags]");

    let mut env_owners = BTreeMap::<String, String>::new();
    let mut spelling_owners = BTreeMap::<String, String>::new();

    for (canonical, definition) in flags {
        let definition = definition
            .as_table()
            .unwrap_or_else(|| panic!("flags.{canonical} must be a table"));

        let env = definition
            .get("env")
            .and_then(toml::Value::as_str)
            .unwrap_or_else(|| panic!("flags.{canonical} must declare env"));
        if let Some(previous) = env_owners.insert(env.to_owned(), canonical.to_owned()) {
            panic!(
                "duplicate root global flag env key {env}: flags.{previous} and flags.{canonical}"
            );
        }

        let public = canonical.replace('_', "-");
        if let Some(previous) = spelling_owners.insert(public.clone(), canonical.to_owned()) {
            panic!(
                "duplicate root public CLI spelling --{public}: flags.{previous} and flags.{canonical}"
            );
        }

        if let Some(aliases) = definition.get("aliases").and_then(toml::Value::as_array) {
            for alias in aliases {
                let alias = alias
                    .as_str()
                    .unwrap_or_else(|| panic!("flags.{canonical}.aliases must contain strings"));
                if alias == public {
                    continue;
                }
                if let Some(previous) =
                    spelling_owners.insert(alias.to_owned(), canonical.to_owned())
                {
                    panic!(
                        "duplicate root public CLI spelling --{alias}: flags.{previous} and flags.{canonical}"
                    );
                }
            }
        }
    }
}
