//! Public command model shared by runtime parsing and completion generation.
//!
//! The derived [`crate::cli::Cli`] structure remains the stable internal data
//! model. This module owns compatibility changes to public spellings so one
//! command definition drives parsing, help, and shell completion.

use std::ffi::OsString;

use clap::{Command, CommandFactory, FromArgMatches};

use crate::cli::Cli;

pub const DO_NOT_WRITE_NEW_MANIFEST_ENV: &str =
    "ZED_PKG_DO_NOT_WRITE_NEW_MANIFEST";
pub const LEGACY_ALLOW_NO_MANIFEST_ENV: &str = "ZED_PKG_ALLOW_NO_MANIFEST";

/// Build the established CLI command with the durable-manifest contract's
/// canonical spelling applied to the existing typed argument ID.
pub fn command() -> Command {
    Cli::command().mut_subcommand("install", |install| {
        install.mut_arg("allow_no_manifest", |argument| {
            argument
                .long("do-not-write-new-manifest")
                .visible_aliases(["allow-no-manifest", "skip-manifest"])
                .help(
                    "Do not create a new .zpkg.toml when installing into a project without one",
                )
        })
    })
}

/// Parse the process arguments through the exact command model used by help
/// and completion generation.
pub fn parse() -> Cli {
    let matches = command().get_matches();
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

/// Apply environment compatibility before flags2env and Clap read process
/// configuration, and report deprecated command-line spellings.
///
/// `ZED_PKG_DO_NOT_WRITE_NEW_MANIFEST` is canonical. The old environment key
/// remains functional for scripts while the embedded flags2env contract is
/// migrated without breaking existing deployments.
pub fn prepare_environment(args: &[OsString]) {
    let canonical = std::env::var_os(DO_NOT_WRITE_NEW_MANIFEST_ENV);
    let legacy = std::env::var_os(LEGACY_ALLOW_NO_MANIFEST_ENV);

    match (canonical, legacy) {
        (Some(value), Some(_)) => {
            eprintln!(
                "warning: both {DO_NOT_WRITE_NEW_MANIFEST_ENV} and deprecated \
                 {LEGACY_ALLOW_NO_MANIFEST_ENV} are set; using \
                 {DO_NOT_WRITE_NEW_MANIFEST_ENV}"
            );
            // SAFETY: main calls this once at process startup, before worker
            // threads exist and before flags2env or Clap reads the variables.
            unsafe { std::env::set_var(LEGACY_ALLOW_NO_MANIFEST_ENV, value) };
        }
        (Some(value), None) => {
            // SAFETY: see the startup-only argument above.
            unsafe { std::env::set_var(LEGACY_ALLOW_NO_MANIFEST_ENV, value) };
        }
        (None, Some(_)) => {
            eprintln!(
                "warning: {LEGACY_ALLOW_NO_MANIFEST_ENV} is deprecated; use \
                 {DO_NOT_WRITE_NEW_MANIFEST_ENV}"
            );
        }
        (None, None) => {}
    }

    if let Some(flag) = legacy_manifest_flag(args) {
        eprintln!("warning: {flag} is deprecated; use --do-not-write-new-manifest");
    }
}

fn legacy_manifest_flag(args: &[OsString]) -> Option<&'static str> {
    args.iter().find_map(|argument| {
        let argument = argument.to_str()?;
        if option_is(argument, "--allow-no-manifest") {
            Some("--allow-no-manifest")
        } else if option_is(argument, "--skip-manifest") {
            Some("--skip-manifest")
        } else {
            None
        }
    })
}

fn option_is(argument: &str, option: &str) -> bool {
    argument == option
        || argument
            .strip_prefix(option)
            .is_some_and(|tail| tail.starts_with('='))
}

#[cfg(test)]
mod tests {
    use clap::FromArgMatches;

    use super::*;
    use crate::cli::Cmd;

    #[test]
    fn canonical_and_legacy_spellings_map_to_one_typed_flag() {
        for spelling in [
            "--do-not-write-new-manifest",
            "--allow-no-manifest",
            "--skip-manifest",
        ] {
            let matches = command()
                .try_get_matches_from(["zed", "install", "acme/http-kit@^1", spelling])
                .unwrap();
            let cli = Cli::from_arg_matches(&matches).unwrap();
            match cli.cmd {
                Cmd::Install {
                    specs,
                    allow_no_manifest,
                    ..
                } => {
                    assert_eq!(specs, ["acme/http-kit@^1"]);
                    assert!(allow_no_manifest, "{spelling}");
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn install_help_leads_with_the_new_canonical_spelling() {
        let mut install = command()
            .find_subcommand("install")
            .expect("install command")
            .clone();
        let mut output = Vec::new();
        install.write_long_help(&mut output).unwrap();
        let help = String::from_utf8(output).unwrap();
        assert!(help.contains("--do-not-write-new-manifest"));
        assert!(help.contains("--allow-no-manifest"));
        assert!(help.contains("--skip-manifest"));
    }

    #[test]
    fn only_legacy_cli_spellings_trigger_a_deprecation_marker() {
        let args = |value: &str| vec![OsString::from("zed"), OsString::from(value)];
        assert_eq!(
            legacy_manifest_flag(&args("--allow-no-manifest")),
            Some("--allow-no-manifest")
        );
        assert_eq!(
            legacy_manifest_flag(&args("--skip-manifest=true")),
            Some("--skip-manifest")
        );
        assert_eq!(
            legacy_manifest_flag(&args("--do-not-write-new-manifest")),
            None
        );
        assert_eq!(legacy_manifest_flag(&args("--unrelated")), None);
    }

    #[test]
    fn option_matching_does_not_accept_prefix_collisions() {
        assert!(option_is(
            "--allow-no-manifest=true",
            "--allow-no-manifest"
        ));
        assert!(!option_is(
            "--allow-no-manifest-extra",
            "--allow-no-manifest"
        ));
        assert!(!option_is("allow-no-manifest", "--allow-no-manifest"));
    }
}
