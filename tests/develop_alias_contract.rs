use flags2env::{BundledFlags2Env, StructuredParse};
use std::fs;
use std::process::{Command, Output};

const DEVELOP_CONTRACT: &str = include_str!("../.dev-cli-flags.toml");

const CLEAN_ENV: &[&str] = &[
    "ZED_DEV_COMMAND",
    "ZED_DEV_ISOLATED_HOME",
    "ZED_DEV_NIX",
    "ZED_DEV_NIX_ACTIVE",
    "ZED_DEV_NO_INSTALL",
    "ZED_DEV_PRINT_ENV",
    "ZED_DEV_PROFILE",
    "ZED_DEV_PYTHON",
    "ZED_DEV_PYTHON_VENV",
    "ZED_DEV_SHELL",
    "ZED_DEV_VENV",
    "ZED_PKG_ALLOW_BUILD",
    "ZED_PKG_ALLOW_INSTALL_HOOKS",
    "ZED_PKG_ALLOW_NATIVE_DEPS",
    "ZED_PKG_NATIVE_MANAGER",
    "ZED_PKG_FROZEN",
];

fn parse_develop(spelling: &str) -> StructuredParse {
    let directory = tempfile::tempdir().expect("create flags2env contract directory");
    let contract = directory.path().join(".cli-flags.toml");
    fs::write(&contract, DEVELOP_CONTRACT).expect("write embedded develop contract");
    let contract = contract
        .to_str()
        .expect("temporary flags2env contract path must be UTF-8");

    let argv = [
        "zed",
        spelling,
        "--command",
        // This value is itself the alias token. It must remain an option value
        // rather than being reconsidered as another subcommand.
        "dev",
        "--isolated-home",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();

    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract))
        .expect("develop flags2env contract must audit cleanly");
    parser
        .parse_structured(&argv, Some(contract))
        .expect("parse develop command spelling")
}

fn print_env(spelling: &str, root: &std::path::Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zed"));
    command.current_dir(root);
    for key in CLEAN_ENV {
        command.env_remove(key);
    }
    command.args([
        spelling,
        "--no-install",
        "--nix",
        "never",
        "--python-venv",
        "never",
        "--print-env",
    ]);
    command.output().expect("run zed develop spelling")
}

#[test]
fn flags2env_resolves_dev_to_the_canonical_develop_path() {
    let canonical = parse_develop("develop");
    let alias = parse_develop("dev");

    assert!(canonical.errors.is_empty(), "{:?}", canonical.errors);
    assert!(canonical.unknown_options.is_empty());
    assert_eq!(canonical.command, "develop");
    assert_eq!(canonical.subcommands, ["develop"]);
    assert_eq!(
        canonical.flags.get("ZED_DEV_COMMAND").map(String::as_str),
        Some("dev")
    );

    // Aliases are an alternate input spelling only. Every public parse channel
    // reports the same canonical command and the same flags/environment map.
    assert_eq!(alias, canonical);
}

#[test]
fn zed_dev_and_zed_develop_print_the_same_managed_environment() {
    let directory = tempfile::tempdir().expect("create develop alias fixture");
    let canonical = print_env("develop", directory.path());
    let alias = print_env("dev", directory.path());

    assert!(
        canonical.status.success(),
        "zed develop failed: {}",
        String::from_utf8_lossy(&canonical.stderr)
    );
    assert!(
        alias.status.success(),
        "zed dev failed: {}",
        String::from_utf8_lossy(&alias.stderr)
    );
    assert_eq!(alias.stdout, canonical.stdout);
    assert_eq!(alias.stderr, canonical.stderr);
}
