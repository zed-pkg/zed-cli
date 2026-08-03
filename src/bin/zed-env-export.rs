use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

#[path = "../nix_environment_export.rs"]
#[allow(private_interfaces)]
mod nix_environment_export;

use nix_environment_export::{ExportManager, export_environment};

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
    let root = std::env::current_dir()?;
    match cli.command {
        Command::Devbox(args) => run_one(&root, ExportManager::Devbox, args),
        Command::Flox(args) => run_one(&root, ExportManager::Flox, args),
    }
}

fn run_one(root: &Path, manager: ExportManager, args: ExportArgs) -> anyhow::Result<()> {
    let result = export_environment(
        root,
        manager,
        Some(&args.plan),
        args.out.as_deref(),
        args.receipt.as_deref(),
    )?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{} environment {}: {}",
            result.manager.as_str(),
            if result.changed { "exported" } else { "unchanged" },
            result.output_path
        );
        println!("receipt: {}", result.receipt_path);
        println!("environment-plan-sha256: {}", result.environment_plan_sha256);
        println!("output-sha256: {}", result.output_sha256);
    }
    Ok(())
}
