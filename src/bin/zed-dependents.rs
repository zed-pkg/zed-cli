use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use zed_cli::cli::Globals;
use zed_cli::config::Config;
use zed_interfaces::dependents::{AutomationModeV1, ConsumerKindV1};

#[derive(Debug, Parser)]
#[command(name = "zed-dependents", about = "Register unpublished Zed consumers and their locked dependencies")]
struct Args {
    #[command(flatten)]
    globals: Globals,
    #[arg(long, default_value = ".")]
    project: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create .zed/dependents.toml without publishing this repository as a package.
    Init {
        #[arg(long, value_enum, default_value = "server")]
        kind: ConsumerKindArg,
        #[arg(long, value_enum, default_value = "notify-only")]
        automation: AutomationModeArg,
    },
    /// Print the exact .zpkg.lock-derived registration payload.
    Snapshot {
        #[arg(long)]
        json: bool,
    },
    /// Register or renew this consumer heartbeat with the Zed API.
    #[command(alias = "heartbeat")]
    Register {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ConsumerKindArg {
    Server,
    Cli,
    Worker,
    App,
    Library,
    Other,
}

impl From<ConsumerKindArg> for ConsumerKindV1 {
    fn from(value: ConsumerKindArg) -> Self {
        match value {
            ConsumerKindArg::Server => Self::Server,
            ConsumerKindArg::Cli => Self::Cli,
            ConsumerKindArg::Worker => Self::Worker,
            ConsumerKindArg::App => Self::App,
            ConsumerKindArg::Library => Self::Library,
            ConsumerKindArg::Other => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AutomationModeArg {
    Off,
    NotifyOnly,
    DraftPr,
}

impl From<AutomationModeArg> for AutomationModeV1 {
    fn from(value: AutomationModeArg) -> Self {
        match value {
            AutomationModeArg::Off => Self::Off,
            AutomationModeArg::NotifyOnly => Self::NotifyOnly,
            AutomationModeArg::DraftPr => Self::DraftPr,
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let project = if args.project.is_absolute() {
        args.project
    } else {
        std::env::current_dir()?.join(args.project)
    };
    match args.command {
        Command::Init { kind, automation } => {
            zed_cli::dependents::init_config(&project, kind.into(), automation.into())
        }
        Command::Snapshot { json } => zed_cli::dependents::snapshot(&project, json),
        Command::Register { dry_run, json } => {
            let cfg = Config::from_globals(&args.globals)?;
            zed_cli::dependents::register(&project, &cfg, dry_run, json)
        }
    }
}
