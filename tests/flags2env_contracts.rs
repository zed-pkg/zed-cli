use std::{
    fs,
    path::{Path, PathBuf},
};

use flags2env::BundledFlags2Env;

fn is_cli_contract(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == ".cli-flags.toml"
                || (name.starts_with('.') && name.ends_with("-cli-flags.toml"))
        })
}

fn cli_contract_paths() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut paths = fs::read_dir(&root)
        .expect("read repository root")
        .map(|entry| entry.expect("read repository-root entry").path())
        .filter(|path| path.is_file() && is_cli_contract(path))
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

#[test]
fn every_checked_in_cli_contract_passes_flags2env_audit() {
    let paths = cli_contract_paths();
    assert!(
        paths.iter().any(|path| {
            path.file_name().and_then(|name| name.to_str()) == Some(".cli-flags.toml")
        }),
        "repository root must contain .cli-flags.toml"
    );

    let parser = BundledFlags2Env::new();
    for path in paths {
        let display = path.display().to_string();
        let path_str = path
            .to_str()
            .unwrap_or_else(|| panic!("CLI contract path is not UTF-8: {display}"));
        parser
            .audit_config(Some(path_str))
            .unwrap_or_else(|error| panic!("{display}: flags2env audit failed: {error}"));

        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{display}: failed to read contract: {error}"));
        let contract: toml::Value = toml::from_str(&source)
            .unwrap_or_else(|error| panic!("{display}: invalid TOML: {error}"));
        assert_eq!(
            contract
                .get("parse")
                .and_then(|parse| parse.get("allow_unknown"))
                .and_then(toml::Value::as_bool),
            Some(false),
            "{display}: CLI contracts must fail closed with parse.allow_unknown = false"
        );
    }
}
