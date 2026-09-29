//! Modular OCI interoperability routing for the current Zed command architecture.
//!
//! OCI planning, local image-layout materialization, and ORAS transport return
//! before normal Zed registry configuration or project transaction recovery.
//! This preserves the credential and mutation boundaries certified by the OCI
//! black-box suites while allowing the main CLI to evolve independently.

use std::env;
use std::ffi::{OsStr, OsString};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::cli::Globals;
use crate::cli_oci::OciCmd;
use crate::oci_push::OciPushOptions;
use crate::{oci, oci_layout, oci_push};

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
struct OciCli {
    #[command(flatten)]
    globals: Globals,

    #[command(subcommand)]
    command: OciRoot,
}

#[derive(Debug, Subcommand)]
enum OciRoot {
    /// Plan, materialize, and distribute immutable Zed packages through OCI.
    Oci(OciArgs),
}

#[derive(Debug, Args)]
struct OciArgs {
    #[command(subcommand)]
    command: OciCmd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Oci,
    OciHelp {
        help_index: usize,
        target_index: usize,
    },
    RootHelp,
    Existing,
}

/// Route modular startup commands before the repository's established parser.
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    if args.get(1).and_then(|value| value.to_str()) == Some("self-update") {
        ores_clis_core::self_update::run_self_update_cli(
            ores_clis_core::self_update::SelfUpdateConfig::new(
                "zed-pkg",
                "zed-cli",
                "zed",
                env!("CARGO_PKG_VERSION"),
            ),
        );
    }

    match route(&args) {
        Route::Oci => Some(run_cli(args)),
        Route::OciHelp {
            help_index,
            target_index,
        } => {
            let mut rewritten = args;
            rewritten[help_index] = OsString::from("oci");
            rewritten.remove(target_index);
            rewritten.push(OsString::from("--help"));
            Some(run_cli(rewritten))
        }
        Route::RootHelp => Some(print_root_help().map(|()| 0)),
        Route::Existing => None,
    }
}

/// Add `oci` and `self-update` to top-level help and generated shell completion.
pub fn augment_root_command(command: clap::Command) -> clap::Command {
    let mut command = command;
    if command
        .get_subcommands()
        .all(|subcommand| subcommand.get_name() != "self-update")
    {
        let self_update = clap::Command::new("self-update")
            .about("Update zed to a selected release")
            .arg(
                clap::Arg::new("version")
                    .value_name("VERSION")
                    .required(false),
            )
            .arg(
                clap::Arg::new("interactive")
                    .long("interactive")
                    .action(clap::ArgAction::SetTrue)
                    .help("Always prompt before replacing the executable"),
            )
            .arg(
                clap::Arg::new("non-interactive")
                    .long("non-interactive")
                    .action(clap::ArgAction::SetTrue)
                    .conflicts_with("interactive")
                    .help("Never prompt before replacing the executable"),
            )
            .arg(
                clap::Arg::new("yes")
                    .short('y')
                    .long("yes")
                    .action(clap::ArgAction::SetTrue)
                    .conflicts_with("interactive")
                    .help("Assume yes without prompting"),
            )
            .arg(
                clap::Arg::new("check")
                    .long("check")
                    .action(clap::ArgAction::SetTrue)
                    .help("Report whether an update is available"),
            )
            .arg(
                clap::Arg::new("dry-run")
                    .long("dry-run")
                    .action(clap::ArgAction::SetTrue)
                    .help("Resolve the release without replacing the binary"),
            )
            .arg(
                clap::Arg::new("verify")
                    .long("verify")
                    .action(clap::ArgAction::SetTrue)
                    .conflicts_with("no-verify")
                    .help("Require SHA-256 verification (the Zed default)"),
            )
            .arg(
                clap::Arg::new("no-verify")
                    .long("no-verify")
                    .action(clap::ArgAction::SetTrue)
                    .conflicts_with("verify")
                    .help("Request unverified updates; Zed policy rejects mutating use"),
            )
            .arg(
                clap::Arg::new("json")
                    .long("json")
                    .action(clap::ArgAction::SetTrue)
                    .help("Emit machine-readable status"),
            );
        command = command.subcommand(self_update);
    }

    if command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "oci")
    {
        return command;
    }

    let oci = OciCli::command()
        .find_subcommand("oci")
        .expect("OCI command is declared")
        .clone();
    command.subcommand(oci)
}

fn print_root_help() -> Result<()> {
    let mut command = augment_root_command(crate::git_submodules::augment_root_command(
        crate::nix_bundle_write::augment_root_command(
            crate::nix_export_plan::augment_root_command(crate::fetch::augment_root_command(
                crate::dev::augment_root_command(crate::cli_model::command()),
            )),
        ),
    ));
    command.print_help().context("printing zed help")?;
    println!();
    Ok(())
}

fn run_cli(args: Vec<OsString>) -> Result<i32> {
    normalize_boolean_environment()?;
    let cli = match OciCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error.print().context("printing OCI argument error")?;
            return Ok(code);
        }
    };

    let cwd = env::current_dir().context("reading current directory")?;
    let OciCli { globals, command } = cli;
    let OciRoot::Oci(oci_args) = command;
    match oci_args.command {
        OciCmd::Plan {
            destination,
            target,
            out,
            json,
        } => {
            if let Some(out) = out {
                oci_layout::materialize(&cwd, &destination, target.as_deref(), &out, json)?;
            } else {
                oci::plan(&cwd, &destination, target.as_deref(), json)?;
            }
        }
        OciCmd::Push {
            layout,
            destination,
            oras,
            username,
            password_stdin,
            registry_config,
            anonymous,
            plain_http,
            insecure_tls,
            ca_file,
            allow_tag_replacement,
            json,
        } => {
            oci_push::push(OciPushOptions {
                layout: &layout,
                destination: &destination,
                oras: &oras,
                username: username.as_deref(),
                password_stdin,
                registry_config: registry_config.as_deref(),
                anonymous,
                plain_http,
                insecure_tls,
                ca_file: ca_file.as_deref(),
                allow_tag_replacement,
                interactive: globals.interactive,
                json,
            })?;
        }
    }
    Ok(0)
}

fn normalize_boolean_environment() -> Result<()> {
    for key in [
        "ZED_PKG_INTERACTIVE",
        "ZED_PKG_OCI_JSON",
        "ZED_PKG_OCI_PUSH_JSON",
        "ZED_PKG_OCI_PASSWORD_STDIN",
        "ZED_PKG_OCI_ANONYMOUS",
        "ZED_PKG_OCI_PLAIN_HTTP",
        "ZED_PKG_OCI_INSECURE_TLS",
        "ZED_PKG_OCI_ALLOW_TAG_REPLACEMENT",
    ] {
        let Some(raw) = env::var_os(key) else {
            continue;
        };
        let raw = raw
            .to_str()
            .with_context(|| format!("boolean environment variable `{key}` is not UTF-8"))?;
        let normalized = match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => "true",
            "false" | "0" | "no" | "off" => "false",
            _ => bail!(
                "boolean environment variable `{key}` must be true/false, 1/0, yes/no, or on/off"
            ),
        };
        if raw != normalized {
            // SAFETY: modular dispatch runs once at process startup before
            // worker threads are created.
            unsafe { env::set_var(key, normalized) };
        }
    }
    Ok(())
}

fn route(args: &[OsString]) -> Route {
    let help_requested = args
        .iter()
        .skip(1)
        .any(|value| value == OsStr::new("--help") || value == OsStr::new("-h"));
    let Some((command_index, command)) = first_command(args) else {
        return if help_requested {
            Route::RootHelp
        } else {
            Route::Existing
        };
    };

    match command.as_str() {
        "oci" => Route::Oci,
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target)) if target == "oci" => Route::OciHelp {
                help_index: command_index,
                target_index,
            },
            None => Route::RootHelp,
            _ => Route::Existing,
        },
        _ => Route::Existing,
    }
}

fn first_command(args: &[OsString]) -> Option<(usize, String)> {
    let mut index = 1;
    while index < args.len() {
        let token = args[index].to_string_lossy();
        if token == "--" {
            return next_positional(args, index + 1);
        }
        if global_option_takes_value(&token) {
            index += if token.contains('=') { 1 } else { 2 };
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((index, token.into_owned()));
    }
    None
}

fn next_positional(args: &[OsString], mut index: usize) -> Option<(usize, String)> {
    while index < args.len() {
        let token = args[index].to_string_lossy();
        if !token.starts_with('-') {
            return Some((index, token.into_owned()));
        }
        index += 1;
    }
    None
}

fn global_option_takes_value(token: &str) -> bool {
    const OPTIONS: &[&str] = &[
        "--registry",
        "--home",
        "--token",
        "--auth-url",
        "--supabase-url",
        "--supabase-key",
    ];
    OPTIONS.iter().any(|option| {
        token == *option
            || token
                .strip_prefix(option)
                .is_some_and(|remainder| remainder.starts_with('='))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn modular_route_and_help_include_oci_without_hiding_other_interop() {
        assert_eq!(
            route(&args(&[
                "zed",
                "oci",
                "plan",
                "oci://ghcr.io/acme/tool:1.2.3",
            ])),
            Route::Oci
        );
        assert!(matches!(
            route(&args(&["zed", "help", "oci", "push"])),
            Route::OciHelp { .. }
        ));
        assert_eq!(route(&args(&["zed", "install"])), Route::Existing);
        assert_eq!(
            route(&args(&["zed", "interop", "nix", "bundle", "write"])),
            Route::Existing
        );

        let command = augment_root_command(crate::git_submodules::augment_root_command(
            crate::nix_bundle_write::augment_root_command(
                crate::nix_export_plan::augment_root_command(crate::fetch::augment_root_command(
                    crate::dev::augment_root_command(crate::cli_model::command()),
                )),
            ),
        ));
        let oci = command
            .get_subcommands()
            .find(|command| command.get_name() == "oci")
            .expect("OCI command must be present");
        assert!(
            oci.get_subcommands()
                .any(|command| command.get_name() == "plan")
        );
        assert!(
            oci.get_subcommands()
                .any(|command| command.get_name() == "push")
        );
        let self_update = command
            .get_subcommands()
            .find(|command| command.get_name() == "self-update")
            .expect("self-update command must be present");
        for option in [
            "interactive",
            "non-interactive",
            "yes",
            "check",
            "dry-run",
            "verify",
            "no-verify",
            "json",
        ] {
            assert!(
                self_update
                    .get_arguments()
                    .any(|argument| argument.get_id().as_str() == option),
                "self-update help is missing {option}"
            );
        }
        let interop = command
            .get_subcommands()
            .find(|command| command.get_name() == "interop")
            .expect("Nix interop command must remain present");
        assert!(
            interop
                .get_subcommands()
                .any(|command| command.get_name() == "nix")
        );
    }

    #[test]
    fn boolean_environment_is_normalized_for_modular_parser() {
        let key = "ZED_PKG_OCI_ANONYMOUS";
        // SAFETY: unit test runs without worker threads and restores the value.
        unsafe { env::set_var(key, "yes") };
        normalize_boolean_environment().unwrap();
        assert_eq!(env::var(key).unwrap(), "true");
        // SAFETY: see above.
        unsafe { env::remove_var(key) };
    }
}
