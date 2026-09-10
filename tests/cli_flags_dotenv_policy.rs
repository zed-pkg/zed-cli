use std::{fs, path::PathBuf};

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
fn every_cli_contract_disables_implicit_working_directory_dotenv_loading() {
    let paths = cli_contract_paths();
    assert!(paths.len() >= 6, "expected all package-owned CLI contracts");

    for path in paths {
        let display = path.display().to_string();
        let source =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {display}: {error}"));
        let contract: toml::Value =
            toml::from_str(&source).unwrap_or_else(|error| panic!("parse {display}: {error}"));
        let env = contract
            .get("env")
            .and_then(toml::Value::as_table)
            .unwrap_or_else(|| {
                panic!(
                    "{display} must declare [env] explicitly; flags2env otherwise reads ./.env from the caller working directory"
                )
            });
        assert_eq!(
            env.get("dotenv").and_then(toml::Value::as_bool),
            Some(false),
            "{display} must set [env].dotenv = false so the currently pinned flags2env generation disables caller-directory dotenv loading",
        );
        let files = env
            .get("files")
            .and_then(toml::Value::as_array)
            .unwrap_or_else(|| {
                panic!(
                    "{display} must set [env].files = [] so current flags2env generations also disable implicit working-directory dotenv loading"
                )
            });
        assert!(
            files.is_empty(),
            "{display} must not implicitly load dotenv files; explicit future opt-in requires separate review"
        );
    }
}
