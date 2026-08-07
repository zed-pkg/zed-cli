use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use zed_cli::asdf_environment::{import_asdf, print_import, print_verification};

#[derive(Debug, Parser)]
#[command(
    name = "zed-asdf",
    version,
    about = "Project-local asdf interoperability adapter for zed-pkg"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Import `.tool-versions` and optional immutable provenance as an EnvironmentPlan.
    Import(EnvironmentArgs),
    /// Verify exact plugin, version, source, and artifact provenance.
    Verify(EnvironmentArgs),
}

#[derive(Debug, Args)]
struct EnvironmentArgs {
    /// Project-local `.tool-versions`; defaults to the current directory.
    #[arg(long, env = "ZED_PKG_ENV_CONFIG")]
    config: Option<PathBuf>,
    /// Zed-owned asdf provenance sidecar; defaults to `.zed/asdf.lock.toml`.
    #[arg(long, env = "ZED_PKG_ENV_LOCK")]
    lock: Option<PathBuf>,
    /// Require exact portable plugin and artifact provenance.
    #[arg(long, env = "ZED_PKG_FROZEN")]
    frozen: bool,
    /// Emit machine-readable JSON.
    #[arg(long, env = "ZED_PKG_ENV_JSON")]
    json: bool,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    match cli.command {
        Command::Import(args) => {
            let imported = import_asdf(
                &cwd,
                args.config.as_deref(),
                args.lock.as_deref(),
                args.frozen,
            )?;
            print_import(&imported, args.json)
        }
        Command::Verify(args) => {
            let imported = import_asdf(
                &cwd,
                args.config.as_deref(),
                args.lock.as_deref(),
                args.frozen,
            )?;
            print_verification(&imported, args.json)
        }
    }
}
