use std::env;
use std::ffi::{OsStr, OsString};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::cli::Globals;
use crate::config::Config;

/// Marker arguments for the modular takeover command. Git-submodule mode is
/// owned by the shared global CLI contract in [`Globals`].
#[derive(Debug, Clone, Args)]
pub struct OvertakeArgs {}

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
pub(super) struct OvertakeCli {
    #[command(flatten)]
    pub(super) globals: Globals,

    #[command(subcommand)]
    pub(super) command: OvertakeCommand,
}

#[derive(Debug, Subcommand)]
pub(super) enum OvertakeCommand {
    /// Adopt Git submodule packages into `.zpkg.toml` and `.zpkg.lock`.
    Overtake(OvertakeArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Route {
    Overtake,
    OvertakeHelp {
        help_index: usize,
        target_index: usize,
    },
    RootHelp,
    Existing,
}

/// Route the modular `overtake` command and augmented root help. Established
/// commands continue through the repository's existing [`crate::cli::Cmd`].
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    match route(&args) {
        Route::Overtake => Some(run_cli(args)),
        Route::OvertakeHelp {
            help_index,
            target_index,
        } => {
            let mut rewritten = args;
            rewritten[help_index] = OsString::from("overtake");
            rewritten.remove(target_index);
            rewritten.push(OsString::from("--help"));
            Some(run_cli(rewritten))
        }
        Route::RootHelp => Some(print_root_help().map(|()| 0)),
        Route::Existing => None,
    }
}

/// Add `overtake` to top-level help and generated shell completions.
pub fn augment_root_command(command: clap::Command) -> clap::Command {
    if command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "overtake")
    {
        return command;
    }

    let overtake = <OvertakeArgs as Args>::augment_args(
        clap::Command::new("overtake")
            .about("Adopt Git submodule packages into .zpkg.toml and .zpkg.lock"),
    );
    command.subcommand(overtake)
}

fn print_root_help() -> Result<()> {
    let mut command = augment_root_command(crate::nix_export_plan::augment_root_command(
        crate::fetch::augment_root_command(crate::dev::augment_root_command(
            crate::cli_model::command(),
        )),
    ));
    command.print_help().context("printing zed help")?;
    println!();
    Ok(())
}

fn run_cli(args: Vec<OsString>) -> Result<i32> {
    crate::flags::normalize_global_boolean_environment(&args)?;
    let cli = match OvertakeCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error
                .print()
                .context("printing zed overtake argument error")?;
            return Ok(code);
        }
    };
    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = env::current_dir().context("reading the current directory")?;
    match cli.command {
        OvertakeCommand::Overtake(_) => {
            if !cli.globals.git_submodules {
                bail!(
                    "no takeover source selected; pass `--git-submodules` or set ZED_PKG_GIT_SUBMODULES=1"
                );
            }
            let report = super::overtake(&cwd, &cfg)?;
            println!(
                "overtook {} Git submodule package(s) in {}",
                report.adopted,
                report.project.display()
            );
            Ok(0)
        }
    }
}

pub(super) fn route(args: &[OsString]) -> Route {
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
        "overtake" => Route::Overtake,
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target)) if target == "overtake" => Route::OvertakeHelp {
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
    use clap::Parser;

    use super::OvertakeCli;

    #[test]
    fn takeover_accepts_the_global_switch_before_and_after_the_command() {
        for args in [
            ["zed", "--git-submodules", "overtake"],
            ["zed", "overtake", "--git-submodules"],
        ] {
            let cli = OvertakeCli::try_parse_from(args).unwrap();
            assert!(cli.globals.git_submodules, "{args:?}");
        }

        let cli =
            OvertakeCli::try_parse_from(["zed", "overtake", "--git-submodules=false"]).unwrap();
        assert!(!cli.globals.git_submodules);
    }
}
