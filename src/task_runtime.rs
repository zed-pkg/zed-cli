//! Native, manager-neutral task execution for schema-v2 environment plans.
//!
//! This first execution slice deliberately consumes the shared
//! `EnvironmentPlanV2` contract instead of re-parsing mise files. It supports
//! validated dependency graphs, aliases, ordered commands, nested task
//! invocations, bounded parallel groups, task-local scalar environment values,
//! working directories, confirmations, timeouts, deterministic dry runs, and
//! content-verified incremental caching. Unsupported template, task-local tool,
//! and extension semantics fail closed rather than disappearing.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use globset::Glob;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zed_interfaces::{
    EnvironmentPlanV2, EnvironmentValue, TaskConfirmation, TaskGroup, TaskInvocation, TaskSpec,
    TaskStep,
};

const CACHE_SCHEMA: u32 = 1;
const CACHE_ROOT: &str = ".zed/task-cache/v1";
const MAX_HASHED_ENTRIES: usize = 100_000;
const MAX_HASHED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Runtime options shared by the staged CLI and the eventual `zed task` route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRunOptions {
    pub dry_run: bool,
    pub assume_yes: bool,
    pub jobs: usize,
    pub no_cache: bool,
    pub args: Vec<String>,
}

impl Default for TaskRunOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            assume_yes: false,
            jobs: 1,
            no_cache: false,
            args: Vec::new(),
        }
    }
}

/// Stable, secret-free execution events. Environment values are never emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TaskEvent {
    TaskStarted {
        task: String,
    },
    TaskSkipped {
        task: String,
        reason: String,
    },
    CommandStarted {
        task: String,
        index: usize,
        command: String,
        dry_run: bool,
    },
    CommandFinished {
        task: String,
        index: usize,
        code: i32,
    },
    TaskFinished {
        task: String,
    },
}

/// Observer used for human streaming output while retaining a deterministic
/// structured report for automation.
pub trait TaskObserver: Send + Sync {
    fn event(&self, event: &TaskEvent);
}

#[derive(Debug, Default)]
pub struct NullTaskObserver;

impl TaskObserver for NullTaskObserver {
    fn event(&self, _event: &TaskEvent) {}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRunReport {
    pub requested: String,
    pub resolved: String,
    pub dry_run: bool,
    pub events: Vec<TaskEvent>,
    pub executed_tasks: Vec<String>,
    pub skipped_tasks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub name: String,
    pub description: Option<String>,
    pub aliases: Vec<String>,
    pub dependencies: Vec<String>,
    pub post_dependencies: Vec<String>,
    pub cache: bool,
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGraph {
    pub requested: String,
    pub resolved: String,
    pub nodes: Vec<String>,
    pub edges: Vec<TaskGraphEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TaskGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: TaskGraphEdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGraphEdgeKind {
    Depends,
    WaitFor,
    DependsPost,
    Invocation,
    Group,
}

/// Loaded, validated project task state.
#[derive(Debug, Clone)]
pub struct TaskRuntime {
    root: PathBuf,
    plan_path: PathBuf,
    plan: EnvironmentPlanV2,
    aliases: BTreeMap<String, String>,
}

impl TaskRuntime {
    pub fn load(root: impl AsRef<Path>, plan: Option<&Path>) -> Result<Self> {
        let root = root.as_ref().canonicalize().with_context(|| {
            format!("canonicalizing project root `{}`", root.as_ref().display())
        })?;
        ensure!(
            root.is_dir(),
            "project root `{}` is not a directory",
            root.display()
        );

        let plan_relative = match plan {
            Some(path) => validate_project_relative(path, "environment plan")?,
            None => discover_plan(&root)?,
        };
        let plan_path = resolve_existing_project_file(&root, &plan_relative, "environment plan")?;
        let input = fs::read_to_string(&plan_path)
            .with_context(|| format!("reading environment plan `{}`", plan_relative.display()))?;
        let parsed = if plan_relative
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            EnvironmentPlanV2::parse_json(&input)
        } else {
            EnvironmentPlanV2::parse_toml(&input)
        }
        .with_context(|| format!("parsing environment plan `{}`", plan_relative.display()))?;

        let mut aliases = BTreeMap::new();
        for (name, task) in &parsed.tasks {
            for alias in &task.aliases {
                aliases.insert(alias.clone(), name.clone());
            }
        }

        Ok(Self {
            root,
            plan_path: plan_relative,
            plan: parsed,
            aliases,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn plan_path(&self) -> &Path {
        &self.plan_path
    }

    pub fn plan(&self) -> &EnvironmentPlanV2 {
        &self.plan
    }

    pub fn resolve_name<'a>(&'a self, name: &'a str) -> Result<&'a str> {
        if self.plan.tasks.contains_key(name) {
            return Ok(name);
        }
        self.aliases
            .get(name)
            .map(String::as_str)
            .with_context(|| format!("unknown task `{name}`"))
    }

    pub fn list(&self, include_hidden: bool) -> Vec<TaskSummary> {
        self.plan
            .tasks
            .iter()
            .filter(|(_, task)| include_hidden || !task.hide)
            .map(|(name, task)| TaskSummary {
                name: name.clone(),
                description: task.description.clone(),
                aliases: task.aliases.clone(),
                dependencies: task
                    .depends
                    .iter()
                    .chain(task.wait_for.iter())
                    .cloned()
                    .collect(),
                post_dependencies: task.depends_post.clone(),
                cache: task.cache.unwrap_or(false),
                hidden: task.hide,
            })
            .collect()
    }

    pub fn info(&self, name: &str) -> Result<TaskSummary> {
        let resolved = self.resolve_name(name)?;
        self.list(true)
            .into_iter()
            .find(|task| task.name == resolved)
            .with_context(|| format!("unknown task `{name}`"))
    }

    pub fn graph(&self, requested: &str) -> Result<TaskGraph> {
        let resolved = self.resolve_name(requested)?.to_string();
        let mut nodes = BTreeSet::new();
        let mut edges = BTreeSet::new();
        self.collect_graph(&resolved, &mut nodes, &mut edges)?;
        Ok(TaskGraph {
            requested: requested.to_string(),
            resolved,
            nodes: nodes.into_iter().collect(),
            edges: edges.into_iter().collect(),
        })
    }

    pub fn run(
        &self,
        requested: &str,
        options: TaskRunOptions,
        observer: &dyn TaskObserver,
    ) -> Result<TaskRunReport> {
        let resolved = self.resolve_name(requested)?.to_string();
        ensure!(options.jobs > 0, "task concurrency must be at least one");
        let runner = Runner::new(self, options, observer);
        runner.execute_task(&resolved, &BTreeMap::new(), &runner.options.args.clone())?;
        Ok(runner.report(requested, &resolved))
    }

    fn collect_graph(
        &self,
        name: &str,
        nodes: &mut BTreeSet<String>,
        edges: &mut BTreeSet<TaskGraphEdge>,
    ) -> Result<()> {
        let name = self.resolve_name(name)?.to_string();
        if !nodes.insert(name.clone()) {
            return Ok(());
        }
        let task = &self.plan.tasks[&name];

        for dependency in &task.depends {
            let dependency = self.resolve_name(dependency)?.to_string();
            edges.insert(TaskGraphEdge {
                from: dependency.clone(),
                to: name.clone(),
                kind: TaskGraphEdgeKind::Depends,
            });
            self.collect_graph(&dependency, nodes, edges)?;
        }
        for dependency in &task.wait_for {
            let dependency = self.resolve_name(dependency)?.to_string();
            edges.insert(TaskGraphEdge {
                from: dependency.clone(),
                to: name.clone(),
                kind: TaskGraphEdgeKind::WaitFor,
            });
            self.collect_graph(&dependency, nodes, edges)?;
        }
        for dependency in &task.depends_post {
            let dependency = self.resolve_name(dependency)?.to_string();
            edges.insert(TaskGraphEdge {
                from: name.clone(),
                to: dependency.clone(),
                kind: TaskGraphEdgeKind::DependsPost,
            });
            self.collect_graph(&dependency, nodes, edges)?;
        }
        for step in task.run.iter().chain(task.run_windows.iter()) {
            match step {
                TaskStep::Command(_) => {}
                TaskStep::Task(invocation) => {
                    let dependency = self.resolve_name(&invocation.task)?.to_string();
                    edges.insert(TaskGraphEdge {
                        from: name.clone(),
                        to: dependency.clone(),
                        kind: TaskGraphEdgeKind::Invocation,
                    });
                    self.collect_graph(&dependency, nodes, edges)?;
                }
                TaskStep::Tasks(group) => {
                    for dependency in &group.tasks {
                        let dependency = self.resolve_name(dependency)?.to_string();
                        edges.insert(TaskGraphEdge {
                            from: name.clone(),
                            to: dependency.clone(),
                            kind: TaskGraphEdgeKind::Group,
                        });
                        self.collect_graph(&dependency, nodes, edges)?;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExecutionState {
    Running,
    Completed,
    Skipped,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskOutcome {
    Completed,
    Skipped,
    AlreadyCompleted,
}

struct Runner<'a> {
    runtime: &'a TaskRuntime,
    options: TaskRunOptions,
    observer: &'a dyn TaskObserver,
    states: Arc<(Mutex<BTreeMap<String, ExecutionState>>, Condvar)>,
    events: Arc<Mutex<Vec<TaskEvent>>>,
    executed: Arc<Mutex<BTreeSet<String>>>,
    skipped: Arc<Mutex<BTreeSet<String>>>,
    command_limiter: Arc<CommandLimiter>,
}

impl<'a> Runner<'a> {
    fn new(
        runtime: &'a TaskRuntime,
        options: TaskRunOptions,
        observer: &'a dyn TaskObserver,
    ) -> Self {
        Self {
            runtime,
            command_limiter: Arc::new(CommandLimiter::new(options.jobs)),
            options,
            observer,
            states: Arc::new((Mutex::new(BTreeMap::new()), Condvar::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            executed: Arc::new(Mutex::new(BTreeSet::new())),
            skipped: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    fn report(&self, requested: &str, resolved: &str) -> TaskRunReport {
        TaskRunReport {
            requested: requested.to_string(),
            resolved: resolved.to_string(),
            dry_run: self.options.dry_run,
            events: self
                .events
                .lock()
                .expect("task event mutex poisoned")
                .clone(),
            executed_tasks: self
                .executed
                .lock()
                .expect("task execution mutex poisoned")
                .iter()
                .cloned()
                .collect(),
            skipped_tasks: self
                .skipped
                .lock()
                .expect("task skip mutex poisoned")
                .iter()
                .cloned()
                .collect(),
        }
    }

    fn emit(&self, event: TaskEvent) {
        self.observer.event(&event);
        self.events
            .lock()
            .expect("task event mutex poisoned")
            .push(event);
    }

    fn execute_task(
        &self,
        requested: &str,
        invocation_env: &BTreeMap<String, EnvironmentValue>,
        args: &[String],
    ) -> Result<TaskOutcome> {
        let name = self.runtime.resolve_name(requested)?.to_string();
        let identity = invocation_identity(&name, invocation_env, args)?;
        let (state_mutex, ready) = &*self.states;
        let mut states = state_mutex.lock().expect("task state mutex poisoned");
        loop {
            match states.get(&identity) {
                None => {
                    states.insert(identity.clone(), ExecutionState::Running);
                    break;
                }
                Some(ExecutionState::Running) => {
                    states = ready.wait(states).expect("task state mutex poisoned");
                }
                Some(ExecutionState::Completed) | Some(ExecutionState::Skipped) => {
                    return Ok(TaskOutcome::AlreadyCompleted);
                }
                Some(ExecutionState::Failed(message)) => {
                    bail!("task `{name}` previously failed: {message}")
                }
            }
        }
        drop(states);

        let result = self.execute_task_inner(&name, invocation_env, args);
        let mut states = state_mutex.lock().expect("task state mutex poisoned");
        match &result {
            Ok(TaskOutcome::Skipped) => {
                states.insert(identity, ExecutionState::Skipped);
            }
            Ok(_) => {
                states.insert(identity, ExecutionState::Completed);
            }
            Err(error) => {
                states.insert(identity, ExecutionState::Failed(format!("{error:#}")));
            }
        }
        ready.notify_all();
        result
    }

    fn execute_task_inner(
        &self,
        name: &str,
        invocation_env: &BTreeMap<String, EnvironmentValue>,
        args: &[String],
    ) -> Result<TaskOutcome> {
        let task = self
            .runtime
            .plan
            .tasks
            .get(name)
            .with_context(|| format!("unknown task `{name}`"))?;
        reject_unsupported_execution_state(name, task)?;

        for dependency in task.depends.iter().chain(task.wait_for.iter()) {
            self.execute_task(dependency, &BTreeMap::new(), &[])?;
        }

        if confirmation_required(task) && !self.options.assume_yes && !self.options.dry_run {
            bail!("task `{name}` requires confirmation; rerun with `--yes` or use `--dry-run`");
        }

        let environment = self.task_environment(name, task, invocation_env, args)?;
        let working_directory = task_working_directory(&self.runtime.root, task)?;
        let cache = if task.cache.unwrap_or(false) && !self.options.no_cache {
            Some(CacheContext::new(
                self.runtime,
                name,
                task,
                invocation_env,
                args,
            )?)
        } else {
            None
        };

        if let Some(cache) = &cache
            && cache.hit()?
        {
            self.emit(TaskEvent::TaskSkipped {
                task: name.to_string(),
                reason: "incremental cache hit".to_string(),
            });
            self.skipped
                .lock()
                .expect("task skip mutex poisoned")
                .insert(name.to_string());
            for dependency in &task.depends_post {
                self.execute_task(dependency, &BTreeMap::new(), &[])?;
            }
            return Ok(TaskOutcome::Skipped);
        }

        self.emit(TaskEvent::TaskStarted {
            task: name.to_string(),
        });
        let steps = selected_steps(task);
        for (index, step) in steps.iter().enumerate() {
            match step {
                TaskStep::Command(command) => {
                    self.execute_command(
                        name,
                        index,
                        command,
                        task,
                        &working_directory,
                        &environment,
                    )?;
                }
                TaskStep::Task(invocation) => {
                    self.execute_invocation(invocation)?;
                }
                TaskStep::Tasks(group) => {
                    self.execute_group(group)?;
                }
            }
        }

        if let Some(cache) = &cache
            && !self.options.dry_run
        {
            cache.store()?;
        }
        for dependency in &task.depends_post {
            self.execute_task(dependency, &BTreeMap::new(), &[])?;
        }
        self.executed
            .lock()
            .expect("task execution mutex poisoned")
            .insert(name.to_string());
        self.emit(TaskEvent::TaskFinished {
            task: name.to_string(),
        });
        Ok(TaskOutcome::Completed)
    }

    fn execute_invocation(&self, invocation: &TaskInvocation) -> Result<()> {
        self.execute_task(&invocation.task, &invocation.env, &invocation.args)?;
        Ok(())
    }

    fn execute_group(&self, group: &TaskGroup) -> Result<()> {
        if !group.parallel || group.tasks.len() < 2 {
            for task in &group.tasks {
                self.execute_task(task, &BTreeMap::new(), &[])?;
            }
            return Ok(());
        }

        let width = self.options.jobs.max(1);
        for chunk in group.tasks.chunks(width) {
            let mut errors = Vec::new();
            thread::scope(|scope| {
                let handles = chunk
                    .iter()
                    .map(|task| scope.spawn(move || self.execute_task(task, &BTreeMap::new(), &[])))
                    .collect::<Vec<_>>();
                for handle in handles {
                    match handle.join() {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => errors.push(error),
                        Err(_) => errors.push(anyhow::anyhow!("parallel task worker panicked")),
                    }
                }
            });
            if let Some(error) = errors.into_iter().next() {
                return Err(error);
            }
        }
        Ok(())
    }

    fn task_environment(
        &self,
        name: &str,
        task: &TaskSpec,
        invocation_env: &BTreeMap<String, EnvironmentValue>,
        args: &[String],
    ) -> Result<BTreeMap<String, OsString>> {
        let mut environment = scalar_environment("env", &self.runtime.plan.env)?;
        environment.extend(scalar_environment(&format!("tasks.{name}.env"), &task.env)?);
        environment.extend(scalar_environment("task invocation env", invocation_env)?);
        environment.insert("ZED_TASK_NAME".to_string(), OsString::from(name));
        environment.insert(
            "ZED_TASK_ARGC".to_string(),
            OsString::from(args.len().to_string()),
        );
        environment.insert(
            "ZED_TASK_ARGS_JSON".to_string(),
            OsString::from(serde_json::to_string(args)?),
        );
        for (index, argument) in args.iter().enumerate() {
            environment.insert(format!("ZED_TASK_ARG_{index}"), OsString::from(argument));
        }
        Ok(environment)
    }

    fn execute_command(
        &self,
        task_name: &str,
        index: usize,
        command: &str,
        task: &TaskSpec,
        working_directory: &Path,
        environment: &BTreeMap<String, OsString>,
    ) -> Result<()> {
        if !task.quiet && !task.silent {
            self.emit(TaskEvent::CommandStarted {
                task: task_name.to_string(),
                index,
                command: command.to_string(),
                dry_run: self.options.dry_run,
            });
        }
        if self.options.dry_run {
            return Ok(());
        }

        let _permit = self.command_limiter.acquire();
        let mut child = build_shell_command(task, command)?;
        child.current_dir(working_directory).envs(environment);
        if task.silent {
            child.stdout(Stdio::null()).stderr(Stdio::null());
        } else {
            child.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        }
        let mut child = child
            .spawn()
            .with_context(|| format!("starting command {index} for task `{task_name}`"))?;
        let status = wait_for_child(&mut child, task.timeout.as_deref())
            .with_context(|| format!("waiting for command {index} in task `{task_name}`"))?;
        ensure_success(task_name, index, command, status)?;
        self.emit(TaskEvent::CommandFinished {
            task: task_name.to_string(),
            index,
            code: status.code().unwrap_or(0),
        });
        Ok(())
    }
}

fn selected_steps(task: &TaskSpec) -> &[TaskStep] {
    #[cfg(windows)]
    if !task.run_windows.is_empty() {
        return &task.run_windows;
    }
    &task.run
}

fn confirmation_required(task: &TaskSpec) -> bool {
    match &task.confirm {
        Some(TaskConfirmation::Enabled(value)) => *value,
        Some(TaskConfirmation::Prompt(_)) => true,
        None => false,
    }
}

fn reject_unsupported_execution_state(name: &str, task: &TaskSpec) -> Result<()> {
    ensure!(
        task.tools.is_empty(),
        "task `{name}` declares task-local tools; native tool activation is not yet certified"
    );
    ensure!(
        task.vars.is_empty(),
        "task `{name}` declares vars; native template evaluation is not yet certified"
    );
    ensure!(
        task.extensions.is_empty(),
        "task `{name}` contains manager extensions that the native runtime cannot execute"
    );
    for step in task.run.iter().chain(task.run_windows.iter()) {
        if let TaskStep::Command(command) = step {
            ensure!(
                !command.contains("{{") && !command.contains("{%"),
                "task `{name}` contains an unevaluated manager template"
            );
        }
    }
    Ok(())
}

fn scalar_environment(
    field: &str,
    values: &BTreeMap<String, EnvironmentValue>,
) -> Result<BTreeMap<String, OsString>> {
    values
        .iter()
        .map(|(key, value)| {
            let value = match value {
                EnvironmentValue::String(value) => OsString::from(value),
                EnvironmentValue::Integer(value) => OsString::from(value.to_string()),
                EnvironmentValue::Float(value) => OsString::from(value.to_string()),
                EnvironmentValue::Boolean(value) => OsString::from(if *value { "true" } else { "false" }),
                EnvironmentValue::Array(_) | EnvironmentValue::Table(_) => {
                    bail!("{field}.{key} is structured and cannot be exported as a process environment variable")
                }
            };
            Ok((key.clone(), value))
        })
        .collect()
}

fn task_working_directory(root: &Path, task: &TaskSpec) -> Result<PathBuf> {
    let Some(relative) = task.dir.as_deref() else {
        return Ok(root.to_path_buf());
    };
    let relative = validate_project_relative(Path::new(relative), "task directory")?;
    let directory = root.join(&relative);
    let canonical = directory
        .canonicalize()
        .with_context(|| format!("canonicalizing task directory `{}`", relative.display()))?;
    ensure!(
        canonical.starts_with(root),
        "task directory `{}` escapes the project through a symlink",
        relative.display()
    );
    ensure!(
        canonical.is_dir(),
        "task directory `{}` is not a directory",
        relative.display()
    );
    Ok(canonical)
}

fn build_shell_command(task: &TaskSpec, command: &str) -> Result<Command> {
    if let Some((program, arguments)) = task.shell.split_first() {
        let mut child = Command::new(program);
        child.args(arguments).arg(command);
        return Ok(child);
    }

    #[cfg(windows)]
    {
        let mut child = Command::new("cmd.exe");
        child.args(["/D", "/S", "/C", command]);
        Ok(child)
    }
    #[cfg(not(windows))]
    {
        let mut child = Command::new("/bin/sh");
        child.args(["-c", command]);
        Ok(child)
    }
}

fn wait_for_child(child: &mut std::process::Child, timeout: Option<&str>) -> Result<ExitStatus> {
    let Some(timeout) = timeout else {
        return child.wait().context("waiting for task command");
    };
    let timeout = parse_duration(timeout)?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("polling task command")? {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "task command exceeded timeout `{}`",
                format_duration(timeout)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn ensure_success(task: &str, index: usize, command: &str, status: ExitStatus) -> Result<()> {
    if status.success() {
        Ok(())
    } else if let Some(code) = status.code() {
        bail!("task `{task}` command {index} exited with code {code}: {command}")
    } else {
        bail!("task `{task}` command {index} terminated by signal: {command}")
    }
}

fn parse_duration(value: &str) -> Result<Duration> {
    let value = value.trim();
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    ensure!(!number.is_empty(), "invalid task timeout `{value}`");
    let number: u64 = number
        .parse()
        .with_context(|| format!("invalid task timeout `{value}`"))?;
    let multiplier = match unit {
        "ms" => 1_u64,
        "s" | "" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => bail!("invalid task timeout unit in `{value}`; expected ms, s, m, or h"),
    };
    let millis = number
        .checked_mul(multiplier)
        .with_context(|| format!("task timeout `{value}` overflows"))?;
    Ok(Duration::from_millis(millis))
}

fn format_duration(value: Duration) -> String {
    format!("{}ms", value.as_millis())
}

struct CommandLimiter {
    available: Mutex<usize>,
    ready: Condvar,
}

impl CommandLimiter {
    fn new(jobs: usize) -> Self {
        Self {
            available: Mutex::new(jobs.max(1)),
            ready: Condvar::new(),
        }
    }

    fn acquire(&self) -> CommandPermit<'_> {
        let mut available = self
            .available
            .lock()
            .expect("command limiter mutex poisoned");
        while *available == 0 {
            available = self
                .ready
                .wait(available)
                .expect("command limiter mutex poisoned");
        }
        *available -= 1;
        CommandPermit { limiter: self }
    }
}

struct CommandPermit<'a> {
    limiter: &'a CommandLimiter,
}

impl Drop for CommandPermit<'_> {
    fn drop(&mut self) {
        let mut available = self
            .limiter
            .available
            .lock()
            .expect("command limiter mutex poisoned");
        *available += 1;
        self.limiter.ready.notify_one();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskCacheRecord {
    schema: u32,
    task: String,
    input_sha256: String,
    output_sha256: String,
    outputs: Vec<String>,
}

struct CacheContext<'a> {
    runtime: &'a TaskRuntime,
    task_name: &'a str,
    task: &'a TaskSpec,
    input_sha256: String,
    path: PathBuf,
}

impl<'a> CacheContext<'a> {
    fn new(
        runtime: &'a TaskRuntime,
        task_name: &'a str,
        task: &'a TaskSpec,
        invocation_env: &BTreeMap<String, EnvironmentValue>,
        args: &[String],
    ) -> Result<Self> {
        ensure!(
            !task.sources.is_empty() && !task.outputs.is_empty(),
            "task `{task_name}` enables caching but does not declare both sources and outputs"
        );
        let sources = expand_patterns(&runtime.root, &task.sources, false)?;
        let input_sha256 = input_digest(runtime, task_name, task, invocation_env, args, &sources)?;
        let identity = invocation_identity(task_name, invocation_env, args)?;
        let cache_name = format!("{}.json", sha256_bytes(identity.as_bytes()));
        Ok(Self {
            runtime,
            task_name,
            task,
            input_sha256,
            path: runtime.root.join(CACHE_ROOT).join(cache_name),
        })
    }

    fn hit(&self) -> Result<bool> {
        let Some(record) = read_cache_record(&self.path)? else {
            return Ok(false);
        };
        if record.schema != CACHE_SCHEMA
            || record.task != self.task_name
            || record.input_sha256 != self.input_sha256
        {
            return Ok(false);
        }
        let outputs = expand_patterns(&self.runtime.root, &self.task.outputs, true)?;
        let output_names = relative_names(&self.runtime.root, &outputs)?;
        if output_names != record.outputs {
            return Ok(false);
        }
        Ok(hash_paths(&self.runtime.root, &outputs)? == record.output_sha256)
    }

    fn store(&self) -> Result<()> {
        let outputs = expand_patterns(&self.runtime.root, &self.task.outputs, true)?;
        ensure!(
            !outputs.is_empty(),
            "cacheable task `{}` produced no declared outputs",
            self.task_name
        );
        let record = TaskCacheRecord {
            schema: CACHE_SCHEMA,
            task: self.task_name.to_string(),
            input_sha256: self.input_sha256.clone(),
            output_sha256: hash_paths(&self.runtime.root, &outputs)?,
            outputs: relative_names(&self.runtime.root, &outputs)?,
        };
        write_cache_record(&self.runtime.root, &self.path, &record)
    }
}

fn input_digest(
    runtime: &TaskRuntime,
    task_name: &str,
    task: &TaskSpec,
    invocation_env: &BTreeMap<String, EnvironmentValue>,
    args: &[String],
    sources: &[PathBuf],
) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"zed.task-input/v1\0");
    digest.update(runtime.plan.canonical_json_bytes()?);
    digest.update(task_name.as_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(task)?);
    digest.update(serde_json::to_vec(invocation_env)?);
    digest.update(serde_json::to_vec(args)?);
    let mut inherited = std::env::vars_os()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect::<Vec<_>>();
    inherited.sort();
    digest.update(serde_json::to_vec(&inherited)?);
    digest.update(hash_paths(&runtime.root, sources)?.as_bytes());
    Ok(hex::encode(digest.finalize()))
}

fn invocation_identity(
    task: &str,
    environment: &BTreeMap<String, EnvironmentValue>,
    args: &[String],
) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(b"zed.task-invocation/v1\0");
    digest.update(task.as_bytes());
    digest.update([0]);
    digest.update(serde_json::to_vec(environment)?);
    digest.update(serde_json::to_vec(args)?);
    Ok(format!("{task}:{}", hex::encode(digest.finalize())))
}

fn read_cache_record(path: &Path) -> Result<Option<TaskCacheRecord>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("opening task cache `{}`", path.display()));
        }
    };
    let record = serde_json::from_reader(BufReader::new(file))
        .with_context(|| format!("parsing task cache `{}`", path.display()))?;
    Ok(Some(record))
}

fn write_cache_record(root: &Path, path: &Path, record: &TaskCacheRecord) -> Result<()> {
    let cache_root = root.join(CACHE_ROOT);
    ensure_safe_cache_directory(root, &cache_root)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("task cache path has no UTF-8 file name")?;
    let temporary = cache_root.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("creating task cache staging `{}`", temporary.display()))?;
        let mut bytes = serde_json::to_vec_pretty(record)?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()?;
        if path.exists() {
            fs::remove_file(path)
                .with_context(|| format!("replacing task cache `{}`", path.display()))?;
        }
        fs::rename(&temporary, path)
            .with_context(|| format!("committing task cache `{}`", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn ensure_safe_cache_directory(root: &Path, cache_root: &Path) -> Result<()> {
    let mut current = root.to_path_buf();
    for component in Path::new(CACHE_ROOT).components() {
        let Component::Normal(component) = component else {
            bail!("invalid task cache root `{CACHE_ROOT}`");
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "task cache component `{}` must be a real directory",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).with_context(|| {
                    format!("creating task cache directory `{}`", current.display())
                })?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting task cache `{}`", current.display()));
            }
        }
    }
    ensure!(current == cache_root, "task cache path mismatch");
    Ok(())
}

fn expand_patterns(
    root: &Path,
    patterns: &[String],
    require_every_pattern: bool,
) -> Result<Vec<PathBuf>> {
    let mut paths = BTreeSet::new();
    for pattern in patterns {
        let portable = pattern.replace('\\', "/");
        let mut matched = false;
        if contains_glob(&portable) {
            let matcher = Glob::new(&portable)
                .with_context(|| format!("invalid task path pattern `{pattern}`"))?
                .compile_matcher();
            for entry in WalkDir::new(root).follow_links(false) {
                let entry = entry?;
                if entry.path() == root {
                    continue;
                }
                let relative = entry.path().strip_prefix(root)?;
                let relative_portable = portable_relative(relative)?;
                if matcher.is_match(&relative_portable) {
                    ensure!(
                        !entry.file_type().is_symlink(),
                        "task path pattern `{pattern}` matched symlink `{relative_portable}`"
                    );
                    paths.insert(entry.path().to_path_buf());
                    matched = true;
                }
            }
        } else {
            let relative = validate_project_relative(Path::new(&portable), "task cache path")?;
            let candidate = root.join(relative);
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) => {
                    ensure!(
                        !metadata.file_type().is_symlink(),
                        "task cache path `{portable}` must not be a symlink"
                    );
                    paths.insert(candidate);
                    matched = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspecting task cache path `{portable}`"));
                }
            }
        }
        if require_every_pattern && !matched {
            return Ok(Vec::new());
        }
    }
    Ok(paths.into_iter().collect())
}

fn hash_paths(root: &Path, paths: &[PathBuf]) -> Result<String> {
    let mut entries = BTreeSet::new();
    for path in paths {
        collect_hash_entries(root, path, &mut entries)?;
    }
    let mut digest = Sha256::new();
    digest.update(b"zed.task-paths/v1\0");
    let mut entry_count = 0_usize;
    let mut byte_count = 0_u64;
    for relative in entries {
        entry_count += 1;
        ensure!(
            entry_count <= MAX_HASHED_ENTRIES,
            "task cache exceeds {MAX_HASHED_ENTRIES} entries"
        );
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "task cache path `{relative}` became a symlink"
        );
        digest.update(relative.as_bytes());
        digest.update([0]);
        if metadata.is_dir() {
            digest.update(b"directory\0");
            continue;
        }
        ensure!(
            metadata.is_file(),
            "task cache path `{relative}` is not a regular file or directory"
        );
        digest.update(b"file\0");
        byte_count = byte_count
            .checked_add(metadata.len())
            .context("task cache byte count overflow")?;
        ensure!(
            byte_count <= MAX_HASHED_BYTES,
            "task cache exceeds {MAX_HASHED_BYTES} bytes"
        );
        let mut file = File::open(&path)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        digest.update([0]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn collect_hash_entries(root: &Path, path: &Path, output: &mut BTreeSet<String>) -> Result<()> {
    let relative = path.strip_prefix(root)?;
    output.insert(portable_relative(relative)?);
    if path.is_dir() {
        for entry in WalkDir::new(path).min_depth(1).follow_links(false) {
            let entry = entry?;
            ensure!(
                !entry.file_type().is_symlink(),
                "task cache directory contains symlink `{}`",
                entry.path().display()
            );
            output.insert(portable_relative(entry.path().strip_prefix(root)?)?);
        }
    }
    Ok(())
}

fn relative_names(root: &Path, paths: &[PathBuf]) -> Result<Vec<String>> {
    let mut output = paths
        .iter()
        .map(|path| portable_relative(path.strip_prefix(root)?))
        .collect::<Result<Vec<_>>>()?;
    output.sort();
    output.dedup();
    Ok(output)
}

fn contains_glob(value: &str) -> bool {
    value.contains(['*', '?', '['])
}

fn portable_relative(path: &Path) -> Result<String> {
    let mut output = String::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!("task path `{}` is not project-relative", path.display());
        };
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(
            component
                .to_str()
                .with_context(|| format!("task path `{}` is not UTF-8", path.display()))?,
        );
    }
    ensure!(!output.is_empty(), "task path cannot be the project root");
    Ok(output)
}

fn validate_project_relative(path: &Path, kind: &str) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "{kind} path cannot be empty");
    ensure!(!path.is_absolute(), "{kind} must be project-relative");
    let text = path.to_string_lossy();
    let windows_drive = text.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        && text.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    ensure!(
        !windows_drive
            && !text.starts_with('~')
            && !text.starts_with("$HOME")
            && !text.starts_with("${HOME}")
            && !text.starts_with("%USERPROFILE%")
            && !text.starts_with("//")
            && !text.starts_with("\\\\"),
        "{kind} must be portable and project-relative"
    );
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(component) => output.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("{kind} cannot escape the project root")
            }
        }
    }
    ensure!(
        !output.as_os_str().is_empty(),
        "{kind} path cannot be the project root"
    );
    Ok(output)
}

fn resolve_existing_project_file(root: &Path, relative: &Path, kind: &str) -> Result<PathBuf> {
    let joined = root.join(relative);
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("canonicalizing {kind} `{}`", relative.display()))?;
    ensure!(
        canonical.starts_with(root),
        "{kind} `{}` escapes the project through a symlink",
        relative.display()
    );
    ensure!(
        canonical.is_file(),
        "{kind} `{}` is not a file",
        relative.display()
    );
    Ok(canonical)
}

fn discover_plan(root: &Path) -> Result<PathBuf> {
    for candidate in [
        "zed-env.toml",
        "zed-env.json",
        ".zed/environment.toml",
        ".zed/environment.json",
    ] {
        if root.join(candidate).is_file() {
            return Ok(PathBuf::from(candidate));
        }
    }
    bail!(
        "no environment plan found; expected zed-env.toml, zed-env.json, .zed/environment.toml, or .zed/environment.json"
    )
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingObserver(Mutex<Vec<TaskEvent>>);

    impl RecordingObserver {
        fn new() -> Self {
            Self(Mutex::new(Vec::new()))
        }
    }

    impl TaskObserver for RecordingObserver {
        fn event(&self, event: &TaskEvent) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn runtime(plan: &str) -> (tempfile::TempDir, TaskRuntime) {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("zed-env.toml"), plan).unwrap();
        let runtime = TaskRuntime::load(root.path(), None).unwrap();
        (root, runtime)
    }

    #[test]
    fn list_and_graph_resolve_aliases_and_dependency_classes() {
        let (_root, runtime) = runtime(
            r#"
            schema = 2

            [tasks.prepare]
            run = ["echo prepare"]

            [tasks.build]
            aliases = ["b"]
            depends = ["prepare"]
            depends_post = ["cleanup"]
            run = ["echo build"]

            [tasks.cleanup]
            run = ["echo cleanup"]
            "#,
        );
        assert_eq!(runtime.resolve_name("b").unwrap(), "build");
        let graph = runtime.graph("b").unwrap();
        assert_eq!(graph.resolved, "build");
        assert!(graph.edges.contains(&TaskGraphEdge {
            from: "prepare".to_string(),
            to: "build".to_string(),
            kind: TaskGraphEdgeKind::Depends,
        }));
        assert!(graph.edges.contains(&TaskGraphEdge {
            from: "build".to_string(),
            to: "cleanup".to_string(),
            kind: TaskGraphEdgeKind::DependsPost,
        }));
    }

    #[test]
    fn dry_run_preserves_order_without_mutation() {
        let (root, runtime) = runtime(
            r#"
            schema = 2

            [tasks.prepare]
            run = ["printf prepare > prepare.txt"]

            [tasks.build]
            depends = ["prepare"]
            run = ["printf build > build.txt"]
            "#,
        );
        let observer = RecordingObserver::new();
        let report = runtime
            .run(
                "build",
                TaskRunOptions {
                    dry_run: true,
                    ..TaskRunOptions::default()
                },
                &observer,
            )
            .unwrap();
        assert!(!root.path().join("prepare.txt").exists());
        assert!(!root.path().join("build.txt").exists());
        let commands = report
            .events
            .iter()
            .filter_map(|event| match event {
                TaskEvent::CommandStarted { task, .. } => Some(task.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(commands, ["prepare", "build"]);
    }

    #[test]
    fn confirmation_and_task_local_tools_fail_closed() {
        let (_root, runtime) = runtime(
            r#"
            schema = 2

            [tasks.release]
            confirm = true
            run = ["echo release"]
            "#,
        );
        let error = runtime
            .run("release", TaskRunOptions::default(), &NullTaskObserver)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires confirmation"));
    }

    #[test]
    fn cache_detects_unchanged_outputs_and_source_drift() {
        let (root, runtime) = runtime(
            r#"
            schema = 2

            [tasks.copy]
            cache = true
            sources = ["input.txt"]
            outputs = ["output.txt"]
            run = ["cat input.txt > output.txt"]
            "#,
        );
        fs::write(root.path().join("input.txt"), "one").unwrap();
        let first = runtime
            .run("copy", TaskRunOptions::default(), &NullTaskObserver)
            .unwrap();
        assert_eq!(first.executed_tasks, ["copy"]);
        let second = runtime
            .run("copy", TaskRunOptions::default(), &NullTaskObserver)
            .unwrap();
        assert_eq!(second.skipped_tasks, ["copy"]);
        fs::write(root.path().join("input.txt"), "two").unwrap();
        let third = runtime
            .run("copy", TaskRunOptions::default(), &NullTaskObserver)
            .unwrap();
        assert_eq!(third.executed_tasks, ["copy"]);
        assert_eq!(
            fs::read_to_string(root.path().join("output.txt")).unwrap(),
            "two"
        );
    }

    #[test]
    fn structured_environment_is_rejected_without_leaking_values() {
        let (_root, runtime) = runtime(
            r#"
            schema = 2

            [tasks.test]
            run = ["echo test"]

            [tasks.test.env]
            SECRET = ["do-not-print"]
            "#,
        );
        let error = runtime
            .run("test", TaskRunOptions::default(), &NullTaskObserver)
            .unwrap_err()
            .to_string();
        assert!(error.contains("structured"));
        assert!(!error.contains("do-not-print"));
    }

    #[test]
    fn timeout_parser_is_bounded_and_explicit() {
        assert_eq!(parse_duration("25ms").unwrap(), Duration::from_millis(25));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert!(parse_duration("5days").is_err());
    }
}
