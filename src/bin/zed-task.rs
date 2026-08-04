use std::path::PathBuf;

use anyhow::{Result, ensure};
use clap::{Args, Parser, Subcommand};
use zed_cli::task_runtime::{TaskEvent, TaskObserver, TaskRunOptions, TaskRuntime};

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
    #[arg(long, env = "ZED_TASK_JOBS", default_value_t = 1, value_parser = parse_jobs)]
    jobs: usize,

    /// Disable content-verified incremental cache reads and writes.
    #[arg(long, env = "ZED_TASK_NO_CACHE")]
    no_cache: bool,

    /// Arguments are exposed without shell interpolation through
    /// ZED_TASK_ARGC, ZED_TASK_ARGS_JSON, and ZED_TASK_ARG_<n>.
    #[arg(last = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

fn parse_jobs(value: &str) -> std::result::Result<usize, String> {
    let jobs = value
        .parse::<usize>()
        .map_err(|_| format!("`{value}` is not a valid positive integer"))?;
    if jobs == 0 {
        return Err("task concurrency must be at least one".to_string());
    }
    Ok(jobs)
}

struct HumanObserver;

impl TaskObserver for HumanObserver {
    fn event(&self, event: &TaskEvent) {
        match event {
            TaskEvent::TaskStarted { task } => eprintln!("[task] {task}"),
            TaskEvent::TaskSkipped { task, reason } => {
                eprintln!("[skip] {task}: {reason}")
            }
            TaskEvent::CommandStarted {
                task,
                command,
                dry_run,
                ..
            } => {
                let prefix = if *dry_run { "[dry-run]" } else { "[run]" };
                eprintln!("{prefix} {task}: {command}");
            }
            TaskEvent::CommandFinished { task, code, .. } => {
                eprintln!("[exit] {task}: {code}")
            }
            TaskEvent::TaskFinished { task } => eprintln!("[done] {task}"),
        }
    }
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let root = std::env::current_dir()?;
    let runtime = TaskRuntime::load(&root, cli.plan.as_deref())?;

    match cli.command {
        TaskCommand::List(args) => {
            let tasks = runtime.list(args.all);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else {
                for task in tasks {
                    let aliases = if task.aliases.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", task.aliases.join(", "))
                    };
                    let description = task
                        .description
                        .as_deref()
                        .map(|value| format!(" — {value}"))
                        .unwrap_or_default();
                    println!("{}{}{}", task.name, aliases, description);
                }
            }
        }
        TaskCommand::Info(args) => {
            let task = runtime.info(&args.task)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!("task: {}", task.name);
                if let Some(description) = task.description {
                    println!("description: {description}");
                }
                println!("aliases: {}", display_list(&task.aliases));
                println!("dependencies: {}", display_list(&task.dependencies));
                println!(
                    "post-dependencies: {}",
                    display_list(&task.post_dependencies)
                );
                println!("cache: {}", task.cache);
                println!("hidden: {}", task.hidden);
            }
        }
        TaskCommand::Graph(args) => {
            let graph = runtime.graph(&args.task)?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&graph)?);
            } else {
                println!("{} -> {}", graph.requested, graph.resolved);
                for edge in graph.edges {
                    println!("{} -{:?}-> {}", edge.from, edge.kind, edge.to);
                }
            }
        }
        TaskCommand::Run(args) => {
            ensure!(
                !cli.json || args.dry_run,
                "`--json` task execution currently requires `--dry-run` so child output cannot corrupt the JSON stream"
            );
            let observer = HumanObserver;
            let report = runtime.run(
                &args.task,
                TaskRunOptions {
                    dry_run: args.dry_run,
                    assume_yes: args.yes,
                    jobs: args.jobs,
                    no_cache: args.no_cache,
                    args: args.args,
                },
                &observer,
            )?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
        }
    }
    Ok(())
}

fn display_list(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}
