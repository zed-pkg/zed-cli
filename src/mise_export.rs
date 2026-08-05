//! Deterministic, conflict-safe export from a schema-v2 Zed environment plan
//! to project-local mise configuration.
//!
//! Export is intentionally a projection, not an excuse to discard state. Any
//! plan field that cannot be represented faithfully by the certified mise
//! subset fails with an exact path. `--write` uses the project transaction
//! journal and a deterministic sidecar so hand-edited manager files are never
//! overwritten silently.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zed_interfaces::environment::{ActivationPolicy, EnvironmentValidationMode, ToolRequirement};
use zed_interfaces::environment_v2::{
    EnvironmentPlanV2, EnvironmentValue, TaskConfirmation, TaskSpec, TaskStep, ToolSpec,
    ToolVersion,
};

use crate::transaction::{ProjectTransaction, STAGING_DIR};

const EXPORT_STATE_PATH: &str = ".zed/mise-export-state.json";
const EXPORT_STATE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiseExportMode {
    Print,
    Check,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MiseExportAction {
    Printed,
    Verified,
    Written,
    Unchanged,
}

#[derive(Debug, Clone, Serialize)]
pub struct MiseExportReport {
    pub manager: &'static str,
    pub plan: String,
    pub output: String,
    pub plan_sha256: String,
    pub output_sha256: String,
    pub action: MiseExportAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MiseExportState {
    schema: u32,
    #[serde(default)]
    outputs: BTreeMap<String, MiseExportRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MiseExportRecord {
    plan: String,
    plan_sha256: String,
    output_sha256: String,
}

/// Render, verify, or write one project-local mise projection.
pub fn export_mise(
    cwd: &Path,
    plan_arg: &Path,
    output_arg: &Path,
    mode: MiseExportMode,
) -> Result<MiseExportReport> {
    let root = cwd
        .canonicalize()
        .with_context(|| format!("failed to resolve project root {}", cwd.display()))?;
    let (plan_path, plan_relative) =
        resolve_project_path(&root, plan_arg, "environment plan", true)?;
    let (output_path, output_relative) =
        resolve_project_path(&root, output_arg, "mise output", false)?;
    validate_export_path_relationships(&plan_path, &plan_relative, &output_path, &output_relative)?;

    let plan = read_plan(&plan_path)?;
    plan.validate(EnvironmentValidationMode::Authoring)
        .context("environment plan is not valid for authoring export")?;
    let candidate = render_mise_config(&plan)?;
    let plan_sha256 = digest_plan(&plan)?;
    let output_sha256 = digest_bytes(candidate.as_bytes());

    match mode {
        MiseExportMode::Print => Ok(MiseExportReport {
            manager: "mise",
            plan: plan_relative,
            output: output_relative,
            plan_sha256,
            output_sha256,
            action: MiseExportAction::Printed,
            document: Some(candidate),
        }),
        MiseExportMode::Check => {
            let current = read_regular_file(&output_path, "mise output")?.with_context(|| {
                format!(
                    "mise export is missing `{output_relative}`; run `zed env export mise --plan {plan_relative} --output {output_relative} --write`"
                )
            })?;
            let current_sha256 = digest_bytes(&current);
            ensure!(
                current == candidate.as_bytes(),
                "mise export drift for `{output_relative}`: current sha256 {current_sha256}, expected {output_sha256}; inspect the diff and run export with --write only when Zed still owns the file"
            );
            Ok(MiseExportReport {
                manager: "mise",
                plan: plan_relative,
                output: output_relative,
                plan_sha256,
                output_sha256,
                action: MiseExportAction::Verified,
                document: None,
            })
        }
        MiseExportMode::Write => write_export(MiseWriteRequest {
            root: &root,
            plan_relative: &plan_relative,
            output_path: &output_path,
            output_relative: &output_relative,
            candidate: &candidate,
            plan_sha256: &plan_sha256,
            output_sha256: &output_sha256,
        }),
    }
}

pub fn print_export(report: &MiseExportReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    match report.action {
        MiseExportAction::Printed => {
            print!("{}", report.document.as_deref().unwrap_or_default());
        }
        MiseExportAction::Verified => println!(
            "verified mise export {} (sha256 {})",
            report.output, report.output_sha256
        ),
        MiseExportAction::Written => println!(
            "wrote mise export {} from {} (sha256 {})",
            report.output, report.plan, report.output_sha256
        ),
        MiseExportAction::Unchanged => println!(
            "mise export unchanged: {} (sha256 {})",
            report.output, report.output_sha256
        ),
    }
    Ok(())
}

struct MiseWriteRequest<'a> {
    root: &'a Path,
    plan_relative: &'a str,
    output_path: &'a Path,
    output_relative: &'a str,
    candidate: &'a str,
    plan_sha256: &'a str,
    output_sha256: &'a str,
}

fn write_export(request: MiseWriteRequest<'_>) -> Result<MiseExportReport> {
    let MiseWriteRequest {
        root,
        plan_relative,
        output_path,
        output_relative,
        candidate,
        plan_sha256,
        output_sha256,
    } = request;
    let state_path = root.join(EXPORT_STATE_PATH);
    ensure_no_symlink_components(
        root,
        Path::new(EXPORT_STATE_PATH),
        "mise export state",
        false,
    )?;
    let mut state = load_state(&state_path)?;
    if let Some(existing) = state.outputs.keys().find(|existing| {
        existing.eq_ignore_ascii_case(output_relative) && existing.as_str() != output_relative
    }) {
        bail!(
            "portable mise export path collision: `{output_relative}` conflicts with existing state key `{existing}`"
        );
    }
    let current = read_regular_file(output_path, "mise output")?;
    let unchanged = current
        .as_deref()
        .is_some_and(|bytes| bytes == candidate.as_bytes());

    if let Some(current) = current.as_deref()
        && !unchanged
    {
        let current_sha256 = digest_bytes(current);
        let record = state.outputs.get(output_relative).with_context(|| {
            format!(
                "refusing to overwrite hand-authored `{output_relative}`: no Zed export state exists; move it aside, choose another --output, or make its contents match the deterministic projection before adopting it"
            )
        })?;
        ensure!(
            record.plan == plan_relative,
            "refusing to overwrite `{output_relative}` from `{plan_relative}` because it is owned by plan `{}`",
            record.plan
        );
        ensure!(
            record.output_sha256 == current_sha256,
            "refusing to overwrite edited `{output_relative}`: export state records sha256 {}, current file is {current_sha256}",
            record.output_sha256
        );
    }

    state.outputs.insert(
        output_relative.to_string(),
        MiseExportRecord {
            plan: plan_relative.to_string(),
            plan_sha256: plan_sha256.to_string(),
            output_sha256: output_sha256.to_string(),
        },
    );
    state.schema = EXPORT_STATE_SCHEMA;
    let mut encoded_state = serde_json::to_vec_pretty(&state)?;
    encoded_state.push(b'\n');

    let mut transaction = ProjectTransaction::begin(root)?;
    if !unchanged {
        transaction.backup(output_path)?;
    }
    transaction.backup(&state_path)?;

    if !unchanged {
        fs::create_dir_all(
            output_path
                .parent()
                .context("mise output must have a project-local parent")?,
        )?;
        fs::write(output_path, candidate.as_bytes())
            .with_context(|| format!("writing mise export {}", output_path.display()))?;
    }
    fs::create_dir_all(
        state_path
            .parent()
            .context("mise export state must have a parent")?,
    )?;
    fs::write(&state_path, encoded_state)
        .with_context(|| format!("writing mise export state {}", state_path.display()))?;
    transaction.commit()?;

    Ok(MiseExportReport {
        manager: "mise",
        plan: plan_relative.to_string(),
        output: output_relative.to_string(),
        plan_sha256: plan_sha256.to_string(),
        output_sha256: output_sha256.to_string(),
        action: if unchanged {
            MiseExportAction::Unchanged
        } else {
            MiseExportAction::Written
        },
        document: None,
    })
}

fn validate_export_path_relationships(
    plan_path: &Path,
    plan_relative: &str,
    output_path: &Path,
    output_relative: &str,
) -> Result<()> {
    let plan_folded = plan_relative.to_ascii_lowercase();
    let output_folded = output_relative.to_ascii_lowercase();
    ensure!(
        output_folded != plan_folded,
        "mise output `{output_relative}` cannot overwrite its source environment plan"
    );
    ensure!(
        output_folded != EXPORT_STATE_PATH.to_ascii_lowercase(),
        "mise output cannot target reserved export state `{EXPORT_STATE_PATH}`"
    );
    let staging_folded = STAGING_DIR.to_ascii_lowercase();
    let staging_prefix = format!("{staging_folded}/");
    for (kind, relative, folded) in [
        ("environment plan", plan_relative, plan_folded.as_str()),
        ("mise output", output_relative, output_folded.as_str()),
    ] {
        ensure!(
            folded != staging_folded && !folded.starts_with(&staging_prefix),
            "{kind} cannot target reserved transaction staging `{STAGING_DIR}`: `{relative}`"
        );
    }

    if output_path.exists() {
        let canonical_output = output_path
            .canonicalize()
            .with_context(|| format!("failed to resolve mise output {}", output_path.display()))?;
        let canonical_plan = plan_path.canonicalize().with_context(|| {
            format!("failed to resolve environment plan {}", plan_path.display())
        })?;
        ensure!(
            canonical_output != canonical_plan,
            "mise output resolves to its source environment plan: {}",
            output_path.display()
        );
    }
    Ok(())
}

fn read_plan(path: &Path) -> Result<EnvironmentPlanV2> {
    let input = fs::read_to_string(path)
        .with_context(|| format!("failed to read environment plan {}", path.display()))?;
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => EnvironmentPlanV2::parse_json(&input)
            .with_context(|| format!("failed to parse JSON environment plan {}", path.display())),
        Some("toml") => EnvironmentPlanV2::parse_toml(&input)
            .with_context(|| format!("failed to parse TOML environment plan {}", path.display())),
        other => bail!(
            "environment plan {} must use a .toml or .json extension, got {}",
            path.display(),
            other.unwrap_or("<none>")
        ),
    }
}

fn render_mise_config(plan: &EnvironmentPlanV2) -> Result<String> {
    let plan = plan.normalized();
    ensure!(
        plan.activation == ActivationPolicy::None,
        "unsupported environment plan field `activation`: mise hook export is intentionally disabled; use activation = \"none\" until explicit trust-bound hook export is certified"
    );
    ensure!(
        plan.system_packages.is_empty(),
        "unsupported environment plan field `system-packages`: package-provider translation is not yet lossless"
    );
    ensure!(
        plan.sources.is_empty(),
        "unsupported environment plan field `sources`: manager provenance belongs in the export state/lock integration and cannot be written into mise.toml losslessly yet"
    );
    ensure!(
        plan.extensions.is_empty(),
        "unsupported environment plan field `extensions`: manager extension fields require an explicit certified mapping"
    );

    let mut root = toml::Table::new();
    if !plan.tools.is_empty() {
        root.insert(
            "tools".to_string(),
            toml::Value::Table(export_tool_map(&plan.tools, "tools")?),
        );
    }
    if !plan.env.is_empty() {
        root.insert(
            "env".to_string(),
            toml::Value::Table(export_value_map(&plan.env, "env", false, true)?),
        );
    }
    if !plan.vars.is_empty() {
        root.insert(
            "vars".to_string(),
            toml::Value::Table(export_value_map(&plan.vars, "vars", true, true)?),
        );
    }
    if !plan.tasks.is_empty() {
        let mut tasks = toml::Table::new();
        for (name, task) in &plan.tasks {
            tasks.insert(name.clone(), toml::Value::Table(export_task(name, task)?));
        }
        root.insert("tasks".to_string(), toml::Value::Table(tasks));
    }
    if !plan.platforms.is_empty() {
        let mut platforms = plan.platforms.clone();
        platforms.sort();
        platforms.dedup();
        let mut settings = toml::Table::new();
        settings.insert("lockfile".to_string(), toml::Value::Boolean(true));
        settings.insert(
            "lockfile_platforms".to_string(),
            toml::Value::Array(platforms.into_iter().map(toml::Value::String).collect()),
        );
        root.insert("settings".to_string(), toml::Value::Table(settings));
    }

    ensure!(
        !root.is_empty(),
        "environment plan has no mise-exportable tools, env, vars, tasks, or platforms"
    );
    let mut output = toml::to_string_pretty(&toml::Value::Table(root))?;
    if !output.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

fn export_tool_map(tools: &BTreeMap<String, ToolSpec>, field: &str) -> Result<toml::Table> {
    let mut output = toml::Table::new();
    for (logical_name, spec) in tools {
        let versions = spec.versions();
        ensure!(
            !versions.is_empty(),
            "unsupported empty tool list `{field}.{logical_name}`"
        );
        let mut key: Option<String> = None;
        let mut values = Vec::with_capacity(versions.len());
        for (index, version) in versions.iter().enumerate() {
            let version_field = format!("{field}.{logical_name}.versions[{index}]");
            let candidate_key = mise_tool_key(logical_name, &version.requirement, &version_field)?;
            if let Some(expected) = &key {
                ensure!(
                    expected == &candidate_key,
                    "`{field}.{logical_name}` uses multiple backend identities (`{expected}` and `{candidate_key}`) that cannot share one mise tool key"
                );
            } else {
                key = Some(candidate_key);
            }
            values.push(export_tool_version(version, &version_field)?);
        }
        let key = key.context("tool key was not derived")?;
        ensure!(
            !output.contains_key(&key),
            "mise tool-key collision: environment plan tool `{logical_name}` maps to already exported key `{key}`"
        );
        let value = if values.len() == 1 {
            values.remove(0)
        } else {
            toml::Value::Array(values)
        };
        output.insert(key, value);
    }
    Ok(output)
}

fn mise_tool_key(name: &str, requirement: &ToolRequirement, field: &str) -> Result<String> {
    if let Some(provider) = &requirement.provider {
        ensure_clean(provider, &format!("{field}.provider"))?;
    }
    let key = match &requirement.backend {
        None => {
            ensure!(
                requirement.provider.is_none(),
                "`{field}.provider` cannot be exported without an exact backend identity"
            );
            name.to_string()
        }
        Some(backend) => {
            ensure_clean(backend, &format!("{field}.backend"))?;
            if let Some(provider) = &requirement.provider {
                let actual = backend.split_once(':').map(|(prefix, _)| prefix);
                ensure!(
                    actual == Some(provider.as_str()),
                    "`{field}.provider` `{provider}` disagrees with backend `{backend}`"
                );
            }
            if backend == &format!("core:{name}") {
                name.to_string()
            } else {
                backend.clone()
            }
        }
    };
    ensure_clean(&key, field)?;
    Ok(key)
}

fn export_tool_version(version: &ToolVersion, field: &str) -> Result<toml::Value> {
    let requirement = &version.requirement;
    ensure_clean(&requirement.requirement, &format!("{field}.requirement"))?;
    ensure!(
        requirement.resolved.is_none(),
        "unsupported `{field}.resolved`: exact manager results belong in mise.lock export, not mise.toml"
    );
    ensure!(
        requirement.source.is_none(),
        "unsupported `{field}.source`: immutable source identity belongs in lock export"
    );
    ensure!(
        requirement.checksums.is_empty(),
        "unsupported `{field}.checksums`: artifact checksums belong in lock export"
    );
    ensure!(
        version.extensions.is_empty(),
        "unsupported `{field}.extensions`: no certified mise mapping exists"
    );
    validate_selector_requirement(&requirement.requirement, field)?;

    if version.options.is_empty() && requirement.platforms.is_empty() {
        return Ok(toml::Value::String(requirement.requirement.clone()));
    }

    let mut table = toml::Table::new();
    insert_selector(&mut table, &requirement.requirement, field)?;
    for (name, value) in &version.options {
        ensure_clean(name, &format!("{field}.options key"))?;
        ensure!(
            !is_sensitive_key(name),
            "refusing to serialize sensitive tool option `{field}.options.{name}`"
        );
        ensure!(
            !table.contains_key(name),
            "`{field}.options.{name}` collides with a mise selector field"
        );
        table.insert(
            name.clone(),
            environment_value_to_toml(value, &format!("{field}.options.{name}"), true)?,
        );
    }
    if !requirement.platforms.is_empty() {
        ensure!(
            !table.contains_key("os"),
            "`{field}.options.os` conflicts with `{field}.platforms`"
        );
        let mut platforms = requirement.platforms.clone();
        platforms.sort();
        platforms.dedup();
        table.insert(
            "os".to_string(),
            toml::Value::Array(platforms.into_iter().map(toml::Value::String).collect()),
        );
    }
    Ok(toml::Value::Table(table))
}

fn validate_selector_requirement(requirement: &str, field: &str) -> Result<()> {
    for selector in ["path", "prefix", "ref", "env"] {
        let prefix = format!("{selector}:");
        if let Some(value) = requirement.strip_prefix(&prefix) {
            let selector_field = format!("{field}.{selector}");
            ensure_clean(value, &selector_field)?;
            if selector == "path" {
                validate_relative_argument(Path::new(value), &selector_field)?;
            }
            return Ok(());
        }
    }
    Ok(())
}

fn insert_selector(table: &mut toml::Table, requirement: &str, field: &str) -> Result<()> {
    for selector in ["path", "prefix", "ref"] {
        let prefix = format!("{selector}:");
        if let Some(value) = requirement.strip_prefix(&prefix) {
            ensure_clean(value, &format!("{field}.{selector}"))?;
            table.insert(selector.to_string(), toml::Value::String(value.to_string()));
            return Ok(());
        }
    }
    ensure!(
        !requirement.starts_with("env:"),
        "`{field}.requirement` uses `env:` with table options; current mise table selectors cannot represent that combination losslessly"
    );
    table.insert(
        "version".to_string(),
        toml::Value::String(requirement.to_string()),
    );
    Ok(())
}

fn export_task(name: &str, task: &TaskSpec) -> Result<toml::Table> {
    let field = format!("tasks.{name}");
    ensure!(
        task.extensions.is_empty(),
        "unsupported `{field}.extensions`: no certified mise mapping exists"
    );
    let mut output = toml::Table::new();
    insert_optional_string(&mut output, "description", task.description.as_deref());
    insert_strings(&mut output, "alias", &task.aliases);
    insert_task_steps(&mut output, "run", &task.run, &field)?;
    insert_task_steps(&mut output, "run_windows", &task.run_windows, &field)?;
    insert_strings(&mut output, "depends", &task.depends);
    insert_strings(&mut output, "depends_post", &task.depends_post);
    insert_strings(&mut output, "wait_for", &task.wait_for);
    if !task.env.is_empty() {
        output.insert(
            "env".to_string(),
            toml::Value::Table(export_value_map(
                &task.env,
                &format!("{field}.env"),
                false,
                true,
            )?),
        );
    }
    if !task.vars.is_empty() {
        output.insert(
            "vars".to_string(),
            toml::Value::Table(export_value_map(
                &task.vars,
                &format!("{field}.vars"),
                true,
                true,
            )?),
        );
    }
    if !task.tools.is_empty() {
        output.insert(
            "tools".to_string(),
            toml::Value::Table(export_tool_map(&task.tools, &format!("{field}.tools"))?),
        );
    }
    insert_optional_string(&mut output, "dir", task.dir.as_deref());
    insert_strings(&mut output, "sources", &task.sources);
    insert_strings(&mut output, "outputs", &task.outputs);
    match task.shell.as_slice() {
        [] => {}
        [shell] => {
            output.insert("shell".to_string(), toml::Value::String(shell.clone()));
        }
        _ => bail!(
            "unsupported `{field}.shell`: current mise task TOML accepts one shell string, but the plan contains a program-plus-arguments vector"
        ),
    }
    insert_optional_string(&mut output, "usage", task.usage.as_deref());
    if let Some(confirm) = &task.confirm {
        output.insert(
            "confirm".to_string(),
            match confirm {
                TaskConfirmation::Enabled(value) => toml::Value::Boolean(*value),
                TaskConfirmation::Prompt(value) => toml::Value::String(value.clone()),
            },
        );
    }
    if let Some(cache) = task.cache {
        output.insert("cache".to_string(), toml::Value::Boolean(cache));
    }
    insert_optional_string(&mut output, "timeout", task.timeout.as_deref());
    insert_true(&mut output, "hide", task.hide);
    insert_true(&mut output, "quiet", task.quiet);
    insert_true(&mut output, "silent", task.silent);
    insert_true(&mut output, "raw", task.raw);
    Ok(output)
}

fn insert_task_steps(
    output: &mut toml::Table,
    key: &str,
    steps: &[TaskStep],
    field: &str,
) -> Result<()> {
    if steps.is_empty() {
        return Ok(());
    }
    let commands = steps
        .iter()
        .enumerate()
        .map(|(index, step)| match step {
            TaskStep::Command(command) => Ok(command.clone()),
            TaskStep::Task(invocation) => bail!(
                "unsupported `{field}.{key}[{index}]`: task invocation `{}` requires a certified mise run-entry mapping",
                invocation.task
            ),
            TaskStep::Tasks(_) => bail!(
                "unsupported `{field}.{key}[{index}]`: grouped task invocation requires a certified mise run-entry mapping"
            ),
        })
        .collect::<Result<Vec<_>>>()?;
    output.insert(
        key.to_string(),
        if commands.len() == 1 {
            toml::Value::String(commands[0].clone())
        } else {
            toml::Value::Array(commands.into_iter().map(toml::Value::String).collect())
        },
    );
    Ok(())
}

fn export_value_map(
    values: &BTreeMap<String, EnvironmentValue>,
    field: &str,
    allow_complex: bool,
    reject_sensitive: bool,
) -> Result<toml::Table> {
    let mut output = toml::Table::new();
    for (name, value) in values {
        ensure_clean(name, &format!("{field} key"))?;
        if reject_sensitive {
            ensure!(
                !is_sensitive_key(name),
                "refusing to serialize literal sensitive field `{field}.{name}`; use a future secret-provider reference instead of committed plaintext"
            );
        }
        output.insert(
            name.clone(),
            environment_value_to_toml(value, &format!("{field}.{name}"), allow_complex)?,
        );
    }
    Ok(output)
}

fn environment_value_to_toml(
    value: &EnvironmentValue,
    field: &str,
    allow_complex: bool,
) -> Result<toml::Value> {
    Ok(match value {
        EnvironmentValue::String(value) => toml::Value::String(value.clone()),
        EnvironmentValue::Integer(value) => toml::Value::Integer(*value),
        EnvironmentValue::Float(value) => {
            ensure!(value.is_finite(), "`{field}` contains a non-finite float");
            toml::Value::Float(*value)
        }
        EnvironmentValue::Boolean(value) => toml::Value::Boolean(*value),
        EnvironmentValue::Array(values) => {
            ensure!(
                allow_complex,
                "unsupported complex environment value `{field}`: arrays/tables are reserved for manager directives and cannot be emitted as literal env values safely"
            );
            toml::Value::Array(
                values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        environment_value_to_toml(value, &format!("{field}[{index}]"), true)
                    })
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        EnvironmentValue::Table(values) => {
            ensure!(
                allow_complex,
                "unsupported complex environment value `{field}`: arrays/tables are reserved for manager directives and cannot be emitted as literal env values safely"
            );
            toml::Value::Table(export_value_map(values, field, true, true)?)
        }
    })
}

fn insert_optional_string(output: &mut toml::Table, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        output.insert(key.to_string(), toml::Value::String(value.to_string()));
    }
}

fn insert_strings(output: &mut toml::Table, key: &str, values: &[String]) {
    if !values.is_empty() {
        output.insert(
            key.to_string(),
            toml::Value::Array(values.iter().cloned().map(toml::Value::String).collect()),
        );
    }
}

fn insert_true(output: &mut toml::Table, key: &str, value: bool) {
    if value {
        output.insert(key.to_string(), toml::Value::Boolean(true));
    }
}

fn is_sensitive_key(name: &str) -> bool {
    let normalized = name.to_ascii_uppercase();
    [
        "PASSWORD",
        "PASSWD",
        "SECRET",
        "TOKEN",
        "PRIVATE_KEY",
        "ACCESS_KEY",
        "API_KEY",
        "CREDENTIAL",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn ensure_clean(value: &str, field: &str) -> Result<()> {
    ensure!(
        value == value.trim() && !value.is_empty(),
        "`{field}` must be non-empty and trimmed"
    );
    ensure!(
        !value.chars().any(char::is_control),
        "`{field}` must contain no control characters"
    );
    Ok(())
}

fn digest_plan(plan: &EnvironmentPlanV2) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(b"zed-pkg:mise-export-plan:v1\0");
    hasher.update(plan.canonical_json_bytes()?);
    Ok(hex::encode(hasher.finalize()))
}

fn digest_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn load_state(path: &Path) -> Result<MiseExportState> {
    let Some(bytes) = read_regular_file(path, "mise export state")? else {
        return Ok(MiseExportState {
            schema: EXPORT_STATE_SCHEMA,
            outputs: BTreeMap::new(),
        });
    };
    let state: MiseExportState = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid mise export state {}", path.display()))?;
    ensure!(
        state.schema == EXPORT_STATE_SCHEMA,
        "unsupported mise export state schema {} in {}; supported schema is {}",
        state.schema,
        path.display(),
        EXPORT_STATE_SCHEMA
    );
    for (output, record) in &state.outputs {
        validate_state_path(output, "state output")?;
        validate_state_path(&record.plan, "state plan")?;
        validate_sha256(&record.plan_sha256, "state plan_sha256")?;
        validate_sha256(&record.output_sha256, "state output_sha256")?;
    }
    Ok(state)
}

fn validate_sha256(value: &str, field: &str) -> Result<()> {
    ensure!(
        value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "`{field}` must be a 64-character hexadecimal SHA-256"
    );
    Ok(())
}

fn read_regular_file(path: &Path, kind: &str) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "{kind} must be a regular non-symlink file: {}",
                path.display()
            );
            fs::read(path)
                .map(Some)
                .with_context(|| format!("failed to read {kind} {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect {kind} {}", path.display()))
        }
    }
}

fn resolve_project_path(
    root: &Path,
    requested: &Path,
    kind: &str,
    must_exist: bool,
) -> Result<(PathBuf, String)> {
    let relative = validate_relative_argument(requested, kind)?;
    ensure_no_symlink_components(root, &relative, kind, must_exist)?;
    let path = root.join(&relative);
    if must_exist {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("{kind} does not exist: {}", path.display()))?;
        ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "{kind} must be a regular non-symlink file: {}",
            path.display()
        );
    } else if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "{kind} must be a regular non-symlink file: {}",
            path.display()
        );
    }
    let portable = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .context("export paths must be valid UTF-8")
                .map(ToOwned::to_owned),
            _ => bail!("{kind} must be normalized and project-relative"),
        })
        .collect::<Result<Vec<_>>>()?
        .join("/");
    Ok((path, portable))
}

fn validate_relative_argument(path: &Path, kind: &str) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    let windows_drive = raw.as_bytes().get(1).is_some_and(|byte| *byte == b':')
        && raw.as_bytes().first().is_some_and(u8::is_ascii_alphabetic);
    ensure!(
        !path.is_absolute()
            && !windows_drive
            && !raw.starts_with("//")
            && !raw.starts_with("\\\\")
            && !raw.starts_with('~')
            && !raw.starts_with("$HOME")
            && !raw.starts_with("${HOME}")
            && !raw.starts_with("%USERPROFILE%"),
        "{kind} must be project-relative on every supported platform: {}",
        path.display()
    );
    ensure!(
        !raw.split(['/', '\\']).any(|part| part == ".."),
        "{kind} must be project-relative and cannot escape the project root: {}",
        path.display()
    );
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            _ => bail!(
                "{kind} must contain only normalized project-relative components: {}",
                path.display()
            ),
        }
    }
    ensure!(
        !normalized.as_os_str().is_empty(),
        "{kind} must name a project file"
    );
    Ok(normalized)
}

fn validate_state_path(path: &str, field: &str) -> Result<()> {
    validate_relative_argument(Path::new(path), field).map(|_| ())
}

fn ensure_no_symlink_components(
    root: &Path,
    relative: &Path,
    kind: &str,
    require_leaf: bool,
) -> Result<()> {
    let mut current = root.to_path_buf();
    let components = relative.components().collect::<Vec<_>>();
    let mut missing_suffix = false;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(value) = component else {
            bail!("{kind} must be normalized and project-relative");
        };
        current.push(value);
        let leaf = index + 1 == components.len();
        if missing_suffix {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "{kind} crosses a symlink at {}",
                    current.display()
                );
                if !leaf {
                    ensure!(
                        metadata.is_dir(),
                        "{kind} parent is not a directory: {}",
                        current.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if require_leaf {
                    bail!("{kind} does not exist: {}", current.display());
                }
                missing_suffix = true;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to inspect {kind} {}", current.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::environment::{EnvironmentManager, EnvironmentSource};
    use zed_interfaces::environment_v2::{TaskGroup, TaskInvocation, ToolVersion};

    fn write_plan(root: &Path, plan: &EnvironmentPlanV2) -> PathBuf {
        let path = root.join("zed-env.toml");
        fs::write(&path, plan.to_toml_string().unwrap()).unwrap();
        PathBuf::from("zed-env.toml")
    }

    fn simple_plan() -> EnvironmentPlanV2 {
        let mut plan = EnvironmentPlanV2::default();
        plan.tools.insert(
            "node".to_string(),
            ToolSpec::Many(vec![
                ToolVersion::new(ToolRequirement {
                    requirement: "22.4.0".to_string(),
                    resolved: None,
                    provider: None,
                    backend: None,
                    source: None,
                    checksums: Vec::new(),
                    platforms: Vec::new(),
                }),
                ToolVersion::new(ToolRequirement {
                    requirement: "20.15.1".to_string(),
                    resolved: None,
                    provider: None,
                    backend: None,
                    source: None,
                    checksums: Vec::new(),
                    platforms: Vec::new(),
                }),
            ]),
        );
        plan.env.insert(
            "APP_ENV".to_string(),
            EnvironmentValue::String("test".to_string()),
        );
        plan.vars.insert(
            "release".to_string(),
            EnvironmentValue::Table(BTreeMap::from([
                (
                    "channel".to_string(),
                    EnvironmentValue::String("stable".to_string()),
                ),
                ("retries".to_string(), EnvironmentValue::Integer(3)),
            ])),
        );
        plan.tasks.insert(
            "setup".to_string(),
            TaskSpec {
                description: Some("Restore dependencies".to_string()),
                aliases: vec!["bootstrap".to_string()],
                run: vec![
                    TaskStep::Command("zed install --frozen".to_string()),
                    TaskStep::Command("cargo check".to_string()),
                ],
                depends: vec!["prepare".to_string()],
                env: BTreeMap::from([("RUST_BACKTRACE".to_string(), EnvironmentValue::Integer(1))]),
                ..TaskSpec::default()
            },
        );
        plan.tasks.insert(
            "prepare".to_string(),
            TaskSpec {
                run: vec![TaskStep::Command("echo prepare".to_string())],
                ..TaskSpec::default()
            },
        );
        plan.platforms = vec!["linux-x64".to_string(), "macos-arm64".to_string()];
        plan
    }

    #[test]
    fn deterministic_export_preserves_multi_version_and_command_order() {
        let plan = simple_plan();
        let first = render_mise_config(&plan).unwrap();
        let second = render_mise_config(&plan).unwrap();
        assert_eq!(first, second);
        let value: toml::Value = toml::from_str(&first).unwrap();
        let versions = value["tools"]["node"].as_array().unwrap();
        assert_eq!(versions[0].as_str(), Some("22.4.0"));
        assert_eq!(versions[1].as_str(), Some("20.15.1"));
        let commands = value["tasks"]["setup"]["run"].as_array().unwrap();
        assert_eq!(commands[0].as_str(), Some("zed install --frozen"));
        assert_eq!(commands[1].as_str(), Some("cargo check"));
    }

    #[test]
    fn set_like_presentation_order_does_not_change_semantic_output() {
        let mut first = simple_plan();
        first.platforms = vec![
            "macos-arm64".to_string(),
            "linux-x64".to_string(),
            "macos-arm64".to_string(),
        ];
        first.tasks.get_mut("setup").unwrap().aliases = vec![
            "z-bootstrap".to_string(),
            "a-bootstrap".to_string(),
            "z-bootstrap".to_string(),
        ];
        first.tasks.get_mut("setup").unwrap().depends =
            vec!["prepare".to_string(), "prepare".to_string()];

        let mut second = first.clone();
        second.platforms.reverse();
        second.tasks.get_mut("setup").unwrap().aliases.reverse();
        second.tasks.get_mut("setup").unwrap().depends.reverse();

        assert_eq!(
            render_mise_config(&first).unwrap(),
            render_mise_config(&second).unwrap()
        );
        assert_eq!(digest_plan(&first).unwrap(), digest_plan(&second).unwrap());
    }

    #[test]
    fn write_check_and_unchanged_state_are_conflict_safe() {
        let temp = tempfile::tempdir().unwrap();
        let plan_path = write_plan(temp.path(), &simple_plan());
        let output = Path::new(".mise.toml");
        let written = export_mise(temp.path(), &plan_path, output, MiseExportMode::Write).unwrap();
        assert_eq!(written.action, MiseExportAction::Written);
        assert!(temp.path().join(EXPORT_STATE_PATH).is_file());

        let verified = export_mise(temp.path(), &plan_path, output, MiseExportMode::Check).unwrap();
        assert_eq!(verified.action, MiseExportAction::Verified);

        let unchanged =
            export_mise(temp.path(), &plan_path, output, MiseExportMode::Write).unwrap();
        assert_eq!(unchanged.action, MiseExportAction::Unchanged);

        let collision = export_mise(
            temp.path(),
            &plan_path,
            Path::new(".MISE.TOML"),
            MiseExportMode::Write,
        )
        .unwrap_err();
        assert!(
            collision
                .to_string()
                .contains("portable mise export path collision")
        );
    }

    #[test]
    fn hand_edits_and_unowned_outputs_are_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let plan_path = write_plan(temp.path(), &simple_plan());
        let output = temp.path().join(".mise.toml");
        fs::write(&output, "[tools]\nnode = \"18\"\n").unwrap();
        let error = export_mise(
            temp.path(),
            &plan_path,
            Path::new(".mise.toml"),
            MiseExportMode::Write,
        )
        .unwrap_err();
        assert!(error.to_string().contains("hand-authored"));

        fs::remove_file(&output).unwrap();
        export_mise(
            temp.path(),
            &plan_path,
            Path::new(".mise.toml"),
            MiseExportMode::Write,
        )
        .unwrap();
        fs::write(&output, "# manual edit\n").unwrap();
        let error = export_mise(
            temp.path(),
            &plan_path,
            Path::new(".mise.toml"),
            MiseExportMode::Write,
        )
        .unwrap_err();
        assert!(error.to_string().contains("edited"));
    }

    #[test]
    fn lock_only_and_manager_extension_state_fail_with_exact_paths() {
        let mut plan = simple_plan();
        plan.tools.get_mut("node").unwrap().versions_mut()[0]
            .requirement
            .resolved = Some("22.4.0".to_string());
        let error = render_mise_config(&plan).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("tools.node.versions[0].resolved")
        );

        let mut plan = simple_plan();
        plan.sources.push(EnvironmentSource {
            manager: EnvironmentManager::Mise,
            path: "mise.toml".to_string(),
            lock_path: None,
            digest: None,
        });
        let error = render_mise_config(&plan).unwrap_err();
        assert!(error.to_string().contains("`sources`"));
    }

    #[test]
    fn sensitive_literals_and_complex_env_directives_fail_closed() {
        let mut plan = simple_plan();
        plan.env.insert(
            "API_TOKEN".to_string(),
            EnvironmentValue::String("plaintext".to_string()),
        );
        assert!(
            render_mise_config(&plan)
                .unwrap_err()
                .to_string()
                .contains("sensitive")
        );

        let mut plan = simple_plan();
        plan.env.insert(
            "PATH_PARTS".to_string(),
            EnvironmentValue::Array(vec![EnvironmentValue::String("bin".to_string())]),
        );
        assert!(
            render_mise_config(&plan)
                .unwrap_err()
                .to_string()
                .contains("complex environment value")
        );

        let mut plan = simple_plan();
        plan.vars.insert(
            "release".to_string(),
            EnvironmentValue::Table(BTreeMap::from([(
                "api_token".to_string(),
                EnvironmentValue::String("plaintext".to_string()),
            )])),
        );
        assert!(
            render_mise_config(&plan)
                .unwrap_err()
                .to_string()
                .contains("vars.release.api_token")
        );
    }

    #[test]
    fn path_selectors_are_portable_even_on_the_scalar_fast_path() {
        for requirement in [
            "path:../tool",
            "path:/opt/tool",
            r"path:C:\\tool",
            r"path:\\\\server\\share\\tool",
            "path:~/tool",
            "path:$HOME/tool",
            "path:${HOME}/tool",
            "path:%USERPROFILE%/tool",
        ] {
            let mut plan = simple_plan();
            plan.tools.get_mut("node").unwrap().versions_mut()[0]
                .requirement
                .requirement = requirement.to_string();
            let error = render_mise_config(&plan).unwrap_err().to_string();
            assert!(
                error.contains("project-relative") || error.contains("cannot escape"),
                "unexpected error for {requirement}: {error}"
            );
        }

        let mut plan = simple_plan();
        plan.tools.get_mut("node").unwrap().versions_mut()[0]
            .requirement
            .requirement = "path:vendor/node".to_string();
        let rendered = render_mise_config(&plan).unwrap();
        let value: toml::Value = toml::from_str(&rendered).unwrap();
        let versions = value["tools"]["node"].as_array().unwrap();
        assert_eq!(versions[0].as_str(), Some("path:vendor/node"));
        assert_eq!(versions[1].as_str(), Some("20.15.1"));
    }

    #[test]
    fn task_invocations_and_shell_argument_vectors_fail_closed() {
        let mut plan = simple_plan();
        plan.tasks.get_mut("setup").unwrap().run = vec![TaskStep::Task(TaskInvocation {
            task: "prepare".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        })];
        assert!(
            render_mise_config(&plan)
                .unwrap_err()
                .to_string()
                .contains("task invocation")
        );

        let mut plan = simple_plan();
        plan.tasks.get_mut("setup").unwrap().run = vec![TaskStep::Tasks(TaskGroup {
            tasks: vec!["prepare".to_string()],
            parallel: true,
        })];
        assert!(
            render_mise_config(&plan)
                .unwrap_err()
                .to_string()
                .contains("grouped task invocation")
        );

        let mut plan = simple_plan();
        plan.tasks.get_mut("setup").unwrap().shell = vec!["bash".to_string(), "-c".to_string()];
        assert!(
            render_mise_config(&plan)
                .unwrap_err()
                .to_string()
                .contains("program-plus-arguments")
        );
    }

    #[test]
    fn export_paths_reject_parent_drive_unc_and_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let plan_path = write_plan(temp.path(), &simple_plan());
        for output in [
            "../mise.toml",
            "C:\\mise.toml",
            "\\\\server\\share\\mise.toml",
            "zed-env.toml",
            "ZED-ENV.TOML",
            ".zed/mise-export-state.json",
            ".ZED/MISE-EXPORT-STATE.JSON",
            ".zpkg-staging/mise.toml",
            ".ZPKG-STAGING/mise.toml",
        ] {
            assert!(
                export_mise(
                    temp.path(),
                    &plan_path,
                    Path::new(output),
                    MiseExportMode::Write,
                )
                .is_err()
            );
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(temp.path(), temp.path().join("linked")).unwrap();
            assert!(
                export_mise(
                    temp.path(),
                    &plan_path,
                    Path::new("linked/mise.toml"),
                    MiseExportMode::Write,
                )
                .unwrap_err()
                .to_string()
                .contains("symlink")
            );

            fs::remove_file(temp.path().join("linked")).unwrap();
            std::os::unix::fs::symlink(temp.path(), temp.path().join(".zed")).unwrap();
            assert!(
                export_mise(
                    temp.path(),
                    &plan_path,
                    Path::new("mise.toml"),
                    MiseExportMode::Write,
                )
                .unwrap_err()
                .to_string()
                .contains("mise export state crosses a symlink")
            );
        }
    }
}
