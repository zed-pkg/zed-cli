use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use zed_cli::task_cli::{self, TaskAction};

#[derive(Debug, Parser)]
#[command(
    name = "zed-task",
    version,
    about = "Native schema-v2 task runtime for zed-pkg"
)]
struct Cli {
    /// Project-local schema-v2 environment plan. If omitted, Zed discovers a
    /// conventional zed-env or .zed/environment TOML/JSON file.
    #[arg(long, env = "ZED_TASK_PLAN")]
    plan: Option<PathBuf>,

    /// Emit stable machine-readable JSON. Real command execution requires
    /// human streaming output, so JSON run reports are currently dry-run only.
    #[arg(long, env = "ZED_TASK_JSON")]
    json: bool,

    #[command(subcommand)]
    command: TaskCommand,
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// List project tasks in deterministic name order.
    List(ListArgs),
    /// Show one task's aliases, dependencies, cache policy, and description.
    Info(TaskName),
    /// Print the validated task dependency/invocation graph.
    Graph(TaskName),
    /// Execute one task and its validated dependency graph.
    Run(RunArgs),
}

#[derive(Debug, Args)]
struct ListArgs {
    /// Include tasks marked hidden.
    #[arg(long, env = "ZED_TASK_ALL")]
    all: bool,
}

#[derive(Debug, Args)]
struct TaskName {
    task: String,
}

#[derive(Debug, Args)]
struct RunArgs {
    task: String,

    /// Plan commands, dependencies, confirmations, and cache decisions without
    /// starting a subprocess or mutating the task cache.
    #[arg(long, env = "ZED_TASK_DRY_RUN")]
    dry_run: bool,

    /// Approve tasks that declare a confirmation requirement.
    #[arg(long, env = "ZED_TASK_YES")]
    yes: bool,

    /// Maximum number of concurrently running task commands.
    #[arg(
        long,
        env = "ZED_TASK_JOBS",
        default_value_t = 1,
        value_parser = task_cli::parse_positive_jobs
    )]
    jobs: usize,

    /// Disable content-verified incremental cache reads and writes.
    #[arg(long, env = "ZED_TASK_NO_CACHE")]
    no_cache: bool,

    /// Arguments are exposed without shell interpolation through
    /// ZED_TASK_ARGC, ZED_TASK_ARGS_JSON, and ZED_TASK_ARG_<n>.
    #[arg(last = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let action = match cli.command {
        TaskCommand::List(args) => TaskAction::List { all: args.all },
        TaskCommand::Info(args) => TaskAction::Info { task: args.task },
        TaskCommand::Graph(args) => TaskAction::Graph { task: args.task },
        TaskCommand::Run(args) => TaskAction::Run {
            task: args.task,
            dry_run: args.dry_run,
            yes: args.yes,
            jobs: args.jobs,
            no_cache: args.no_cache,
            args: args.args,
        },
    };
    task_cli::execute_current_dir(cli.plan, cli.json, action)
}
