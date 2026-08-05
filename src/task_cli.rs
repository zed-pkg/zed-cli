//! Shared command/output layer for the canonical `zed task` route and the
//! staged `zed-task` compatibility binary.

use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};

use crate::task_runtime::{TaskEvent, TaskObserver, TaskRunOptions, TaskRuntime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskAction {
    List {
        all: bool,
    },
    Info {
        task: String,
    },
    Graph {
        task: String,
    },
    Run {
        task: String,
        dry_run: bool,
        yes: bool,
        jobs: usize,
        no_cache: bool,
        args: Vec<String>,
    },
}

/// Parse the shared positive concurrency contract used by both binaries.
pub fn parse_positive_jobs(value: &str) -> std::result::Result<usize, String> {
    let jobs = value
        .parse::<usize>()
        .map_err(|_| format!("`{value}` is not a valid positive integer"))?;
    if jobs == 0 {
        return Err("task concurrency must be at least one".to_string());
    }
    Ok(jobs)
}

/// Execute one task command through the shared schema-v2 runtime.
pub fn execute(
    root: &Path,
    plan: Option<&Path>,
    json: bool,
    action: TaskAction,
) -> Result<()> {
    let runtime = TaskRuntime::load(root, plan)?;
    match action {
        TaskAction::List { all } => print_list(&runtime, all, json),
        TaskAction::Info { task } => print_info(&runtime, &task, json),
        TaskAction::Graph { task } => print_graph(&runtime, &task, json),
        TaskAction::Run {
            task,
            dry_run,
            yes,
            jobs,
            no_cache,
            args,
        } => run_task(
            &runtime,
            &task,
            json,
            TaskRunOptions {
                dry_run,
                assume_yes: yes,
                jobs,
                no_cache,
                args,
            },
        ),
    }
}

pub fn execute_current_dir(
    plan: Option<PathBuf>,
    json: bool,
    action: TaskAction,
) -> Result<()> {
    let root = std::env::current_dir()?;
    execute(&root, plan.as_deref(), json, action)
}

fn print_list(runtime: &TaskRuntime, all: bool, json: bool) -> Result<()> {
    let tasks = runtime.list(all);
    if json {
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
    Ok(())
}

fn print_info(runtime: &TaskRuntime, task_name: &str, json: bool) -> Result<()> {
    let task = runtime.info(task_name)?;
    if json {
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
    Ok(())
}

fn print_graph(runtime: &TaskRuntime, task_name: &str, json: bool) -> Result<()> {
    let graph = runtime.graph(task_name)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&graph)?);
    } else {
        println!("{} -> {}", graph.requested, graph.resolved);
        for edge in graph.edges {
            println!("{} -{:?}-> {}", edge.from, edge.kind, edge.to);
        }
    }
    Ok(())
}

fn run_task(
    runtime: &TaskRuntime,
    task_name: &str,
    json: bool,
    options: TaskRunOptions,
) -> Result<()> {
    ensure!(
        !json || options.dry_run,
        "`--json` task execution currently requires `--dry-run` so child output cannot corrupt the JSON stream"
    );
    let observer = HumanObserver;
    let report = runtime.run(task_name, options, &observer)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_parser_rejects_zero_and_invalid_values() {
        assert_eq!(parse_positive_jobs("3"), Ok(3));
        assert!(parse_positive_jobs("0").is_err());
        assert!(parse_positive_jobs("many").is_err());
    }
}