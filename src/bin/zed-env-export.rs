use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use zed_cli::environment_export_cli::{self, ExportOptions};
use zed_cli::nix_environment_export::ExportManager;

#[derive(Debug, Parser)]
#[command(
    name = "zed-env-export",
    version,
    about = "Generate deterministic Devbox and Flox environments from EnvironmentPlan"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Emit a project-local `devbox.json` and Zed-owned provenance receipt.
    Devbox(ExportArgs),
    /// Emit `.flox/env/manifest.toml` and a Zed-owned provenance receipt.
    Flox(ExportArgs),
}

#[derive(Debug, Args)]
struct ExportArgs {
    /// Canonical EnvironmentPlan JSON input.
    #[arg(
        long,
        default_value = ".zed/environment-plan.json",
        env = "ZED_PKG_ENV_PLAN"
    )]
    plan: PathBuf,
    /// Manager output path. Defaults to `devbox.json` or `.flox/env/manifest.toml`.
    #[arg(long, env = "ZED_PKG_ENV_OUT")]
    out: Option<PathBuf>,
    /// Zed-owned deterministic export receipt path.
    #[arg(long, env = "ZED_PKG_ENV_RECEIPT")]
    receipt: Option<PathBuf>,
    /// Emit a stable machine-readable result.
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
    let (manager, args) = match cli.command {
        Command::Devbox(args) => (ExportManager::Devbox, args),
        Command::Flox(args) => (ExportManager::Flox, args),
    };
    environment_export_cli::execute_current_dir(
        manager,
        ExportOptions {
            plan: Some(args.plan),
            output: args.out,
            receipt: args.receipt,
            json: args.json,
        },
    )
}
