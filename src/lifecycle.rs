//! Project-owned lifecycle hooks for Zed package operations.
//!
//! Hook files are discovered by convention under `.zed/` and `.zpkg/`.
//! `.zpkg.toml` can prepend, append, replace, or disable the convention plan.
//! These hooks belong to the checked-out root project; dependency install hooks
//! retain the separate consent and allow-list gates in the installer.

use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Stdio;
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

const MANIFEST_FILE: &str = ".zpkg.toml";
const SKIP_ENV: &str = "ZED_SKIP_LIFECYCLE";
const STACK_ENV: &str = "ZED_LIFECYCLE_STACK";
const CONVENTION_ROOTS: [&str; 4] = [".zed", ".zed/hooks", ".zpkg", ".zpkg/hooks"];
const CONVENTION_SUFFIXES: [&str; 6] = ["", ".sh", ".bash", ".ps1", ".cmd", ".bat"];
const LIFECYCLE_PHASE_NAMES: [&str; 12] = [
    "pre-install",
    "post-install",
    "pre-build",
    "post-build",
    "pre-test",
    "post-test",
    "pre-pack",
    "post-pack",
    "pre-publish",
    "post-publish",
    "pre-uninstall",
    "post-uninstall",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecyclePhase {
    PreInstall,
    PostInstall,
    PreBuild,
    PostBuild,
    PreTest,
    PostTest,
    PrePack,
    PostPack,
    PrePublish,
    PostPublish,
    PreUninstall,
    PostUninstall,
}

impl LifecyclePhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreInstall => "pre-install",
            Self::PostInstall => "post-install",
            Self::PreBuild => "pre-build",
            Self::PostBuild => "post-build",
            Self::PreTest => "pre-test",
            Self::PostTest => "post-test",
            Self::PrePack => "pre-pack",
            Self::PostPack => "post-pack",
            Self::PrePublish => "pre-publish",
            Self::PostPublish => "post-publish",
            Self::PreUninstall => "pre-uninstall",
            Self::PostUninstall => "post-uninstall",
        }
    }
}

impl std::fmt::Display for LifecyclePhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum HookMode {
    #[default]
    Append,
    Prepend,
    Replace,
    Disable,
}

impl<'de> Deserialize<'de> for HookMode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "append" | "supplement" | "supplements" | "complement" | "complements" => {
                Ok(Self::Append)
            }
            "prepend" => Ok(Self::Prepend),
            "replace" | "override" | "overrides" => Ok(Self::Replace),
            "disable" | "disabled" | "off" => Ok(Self::Disable),
            other => Err(serde::de::Error::custom(format!(
                "unsupported lifecycle mode `{other}`; expected append, prepend, replace, or disable"
            ))),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct HookConfig {
    mode: HookMode,
    command: Option<String>,
    commands: Vec<String>,
    shell: Option<String>,
    env: BTreeMap<String, String>,
}

impl HookConfig {
    fn normalized_commands(&self) -> Vec<String> {
        self.command
            .iter()
            .chain(self.commands.iter())
            .map(|command| command.trim())
            .filter(|command| !command.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum HookValue {
    Config(HookConfig),
    Commands(Vec<String>),
    Command(String),
    Enabled(bool),
}

impl HookValue {
    fn into_config(self) -> HookConfig {
        match self {
            Self::Config(config) => config,
            Self::Commands(commands) => HookConfig {
                commands,
                ..HookConfig::default()
            },
            Self::Command(command) => HookConfig {
                command: Some(command),
                ..HookConfig::default()
            },
            Self::Enabled(true) => HookConfig::default(),
            Self::Enabled(false) => HookConfig {
                mode: HookMode::Disable,
                ..HookConfig::default()
            },
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct ManifestLifecycle {
    lifecycle: BTreeMap<String, HookValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HookSource {
    Convention(PathBuf),
    Manifest {
        command: String,
        shell: Option<String>,
        env: BTreeMap<String, String>,
        ordinal: usize,
    },
}

impl HookSource {
    fn label(&self, phase: LifecyclePhase) -> String {
        match self {
            Self::Convention(path) => path.to_string_lossy().replace('\\', "/"),
            Self::Manifest { ordinal, .. } => {
                format!("{MANIFEST_FILE}:lifecycle.{}[{ordinal}]", phase.as_str())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleReport {
    pub phase: LifecyclePhase,
    pub discovered: usize,
    pub executed: usize,
    pub skipped: bool,
}

impl LifecycleReport {
    fn skipped(phase: LifecyclePhase) -> Self {
        Self {
            phase,
            discovered: 0,
            executed: 0,
            skipped: true,
        }
    }
}

pub fn run_phase(project: &Path, phase: LifecyclePhase) -> Result<LifecycleReport> {
    if env_truthy(SKIP_ENV) {
        return Ok(LifecycleReport::skipped(phase));
    }

    let existing_stack = env::var(STACK_ENV).unwrap_or_default();
    if existing_stack
        .split(',')
        .map(str::trim)
        .any(|item| item == phase.as_str())
    {
        eprintln!(
            "warning: skipping recursive Zed lifecycle phase `{phase}` (stack: {existing_stack})"
        );
        return Ok(LifecycleReport::skipped(phase));
    }

    let hooks = resolve_hooks(project, phase)?;
    let discovered = hooks.len();
    if hooks.is_empty() {
        return Ok(LifecycleReport {
            phase,
            discovered,
            executed: 0,
            skipped: false,
        });
    }

    let root = project
        .canonicalize()
        .with_context(|| format!("resolving project root {}", project.display()))?;
    let stack = if existing_stack.trim().is_empty() {
        phase.as_str().to_owned()
    } else {
        format!("{existing_stack},{}", phase.as_str())
    };
    let depth = stack
        .split(',')
        .filter(|item| !item.trim().is_empty())
        .count();

    for (index, hook) in hooks.iter().enumerate() {
        let label = hook.label(phase);
        println!(
            "running Zed {phase} hook {}/{} ({label})",
            index + 1,
            hooks.len()
        );
        let mut command = command_for_hook(&root, hook)?;
        configure_environment(
            &mut command,
            &root,
            phase,
            &label,
            index + 1,
            hooks.len(),
            depth,
            &stack,
        );
        let status = command
            .status()
            .with_context(|| format!("starting Zed {phase} hook {label}"))?;
        ensure_success(status, phase, &label)?;
    }

    Ok(LifecycleReport {
        phase,
        discovered,
        executed: discovered,
        skipped: false,
    })
}

pub fn around<T>(
    project: &Path,
    pre: LifecyclePhase,
    post: LifecyclePhase,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    run_phase(project, pre)?;
    let value = operation()?;
    run_phase(project, post)?;
    Ok(value)
}

fn resolve_hooks(project: &Path, phase: LifecyclePhase) -> Result<Vec<HookSource>> {
    let conventions = discover_conventions(project, phase)?;
    let Some(config) = read_config(project, phase)? else {
        return Ok(conventions);
    };

    if config.mode == HookMode::Disable {
        ensure!(
            config.normalized_commands().is_empty(),
            "lifecycle.{} uses mode=disable but also declares commands",
            phase.as_str()
        );
        return Ok(Vec::new());
    }

    let explicit = config
        .normalized_commands()
        .into_iter()
        .enumerate()
        .map(|(index, command)| HookSource::Manifest {
            command,
            shell: config.shell.clone(),
            env: config.env.clone(),
            ordinal: index + 1,
        })
        .collect::<Vec<_>>();

    Ok(match config.mode {
        HookMode::Append => conventions.into_iter().chain(explicit).collect(),
        HookMode::Prepend => explicit.into_iter().chain(conventions).collect(),
        HookMode::Replace => explicit,
        HookMode::Disable => unreachable!("handled above"),
    })
}

fn read_config(project: &Path, phase: LifecyclePhase) -> Result<Option<HookConfig>> {
    let path = project.join(MANIFEST_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let contents = fs::read_to_string(&path)
        .with_context(|| format!("reading lifecycle configuration from {}", path.display()))?;
    let document: ManifestLifecycle = toml::from_str(&contents)
        .with_context(|| format!("parsing lifecycle configuration in {}", path.display()))?;
    document.lifecycle.keys().try_for_each(|configured_phase| {
        ensure!(
            LIFECYCLE_PHASE_NAMES.contains(&configured_phase.as_str()),
            "unknown lifecycle phase `{configured_phase}` in {}; expected one of {}",
            path.display(),
            LIFECYCLE_PHASE_NAMES.join(", ")
        );
        Ok(())
    })?;
    Ok(document
        .lifecycle
        .get(phase.as_str())
        .cloned()
        .map(HookValue::into_config))
}

fn discover_conventions(project: &Path, phase: LifecyclePhase) -> Result<Vec<HookSource>> {
    let root = project
        .canonicalize()
        .with_context(|| format!("resolving project root {}", project.display()))?;
    let discovered = convention_relatives(phase)
        .map(|relative| convention_hook(project, &root, relative))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten();
    Ok(unique_conventions(discovered))
}

fn convention_relatives(phase: LifecyclePhase) -> impl Iterator<Item = PathBuf> {
    CONVENTION_ROOTS.into_iter().flat_map(move |directory| {
        CONVENTION_SUFFIXES
            .into_iter()
            .map(move |suffix| Path::new(directory).join(format!("{}{suffix}", phase.as_str())))
    })
}

fn convention_hook(
    project: &Path,
    root: &Path,
    relative: PathBuf,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let candidate = project.join(&relative);
    let metadata = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading lifecycle hook {}", candidate.display()));
        }
    };
    ensure!(
        !metadata.file_type().is_symlink(),
        "lifecycle hook {} must not be a symbolic link",
        candidate.display()
    );
    ensure!(
        metadata.is_file(),
        "lifecycle hook {} is not a regular file",
        candidate.display()
    );
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolving lifecycle hook {}", candidate.display()))?;
    ensure!(
        canonical.starts_with(root),
        "lifecycle hook {} resolves outside project root {}",
        candidate.display(),
        root.display()
    );
    Ok(Some((canonical, relative)))
}

fn unique_conventions(discovered: impl IntoIterator<Item = (PathBuf, PathBuf)>) -> Vec<HookSource> {
    discovered
        .into_iter()
        .fold(
            (HashSet::new(), Vec::new()),
            |(mut seen, mut hooks), (canonical, relative)| {
                if seen.insert(canonical) {
                    hooks.push(HookSource::Convention(relative));
                }
                (seen, hooks)
            },
        )
        .1
}

fn command_for_hook(root: &Path, hook: &HookSource) -> Result<Command> {
    match hook {
        HookSource::Convention(relative) => command_for_file(&root.join(relative)),
        HookSource::Manifest {
            command,
            shell,
            env,
            ..
        } => {
            let mut process = command_for_text(shell.as_deref(), command)?;
            process.envs(env);
            Ok(process)
        }
    }
}

#[cfg(unix)]
fn command_for_file(path: &Path) -> Result<Command> {
    use std::os::unix::fs::PermissionsExt;

    let extension = path.extension().and_then(OsStr::to_str).unwrap_or_default();
    if fs::metadata(path)?.permissions().mode() & 0o111 != 0 {
        return Ok(Command::new(path));
    }
    let mut command = match extension {
        "ps1" => {
            let mut command = Command::new("pwsh");
            command.args(["-NoProfile", "-NonInteractive", "-File"]);
            command
        }
        "cmd" | "bat" => {
            let mut command = Command::new("cmd");
            command.arg("/C");
            command
        }
        _ => Command::new("sh"),
    };
    command.arg(path);
    Ok(command)
}

#[cfg(windows)]
fn command_for_file(path: &Path) -> Result<Command> {
    use std::os::windows::process::CommandExt;

    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let native_path = windows_native_path(path);

    match extension.as_str() {
        "ps1" => {
            let mut command = Command::new("powershell");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]);
            command.arg(native_path);
            Ok(command)
        }
        "sh" | "bash" => {
            let mut command = Command::new(if extension == "bash" { "bash" } else { "sh" });
            command.arg(windows_posix_path(&native_path));
            Ok(command)
        }
        "cmd" | "bat" => {
            let mut command = Command::new("cmd");
            command.args(["/D", "/C"]);
            command.raw_arg("\"%ZED_INTERNAL_HOOK_FILE%\"");
            command.env("ZED_INTERNAL_HOOK_FILE", native_path);
            Ok(command)
        }
        "" => {
            let input = fs::File::open(path).with_context(|| {
                format!("opening extensionless lifecycle hook {}", path.display())
            })?;
            let mut command = Command::new("cmd");
            command.args(["/D", "/Q"]);
            command.stdin(Stdio::from(input));
            Ok(command)
        }
        other => bail!(
            "unsupported Windows lifecycle hook extension `{other}` for {}",
            path.display()
        ),
    }
}

#[cfg(windows)]
fn windows_native_path(path: &Path) -> PathBuf {
    let rendered = path.as_os_str().to_string_lossy();
    if let Some(rest) = rendered.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = rendered.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

#[cfg(windows)]
fn windows_posix_path(path: &Path) -> String {
    windows_native_path(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn command_for_text(shell: Option<&str>, text: &str) -> Result<Command> {
    let configured = shell.map(str::trim).filter(|value| !value.is_empty());
    let parts = configured
        .map(|value| value.split_whitespace().collect::<Vec<_>>())
        .unwrap_or_default();

    #[cfg(windows)]
    let default_shell = "cmd";
    #[cfg(not(windows))]
    let default_shell = "sh";

    let executable = parts.first().copied().unwrap_or(default_shell);
    let mut command = Command::new(executable);
    if parts.len() > 1 {
        command.args(&parts[1..]);
    }
    let executable_name = Path::new(executable)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(executable)
        .to_ascii_lowercase();
    if executable_name.contains("powershell") || executable_name == "pwsh" {
        command.args(["-NoProfile", "-NonInteractive", "-Command"]);
    } else if executable_name == "cmd" || executable_name == "cmd.exe" {
        command.arg("/C");
    } else {
        command.arg("-c");
    }
    command.arg(text);
    Ok(command)
}

#[allow(clippy::too_many_arguments)]
fn configure_environment(
    command: &mut Command,
    root: &Path,
    phase: LifecyclePhase,
    source: &str,
    index: usize,
    total: usize,
    depth: usize,
    stack: &str,
) {
    #[cfg(windows)]
    let environment_root = windows_native_path(root);
    #[cfg(not(windows))]
    let environment_root = root.to_path_buf();

    command
        .current_dir(root)
        .env("ZED_LIFECYCLE_PHASE", phase.as_str())
        .env("ZED_LIFECYCLE_HOOK_INDEX", index.to_string())
        .env("ZED_LIFECYCLE_HOOK_TOTAL", total.to_string())
        .env("ZED_LIFECYCLE_DEPTH", depth.to_string())
        .env("ZED_LIFECYCLE_SOURCE", source)
        .env("ZED_PROJECT_ROOT", &environment_root)
        .env("ZED_PKG_ROOT", &environment_root)
        .env("ZED_PACKAGE_MANIFEST", environment_root.join(MANIFEST_FILE))
        .env(STACK_ENV, stack);
}

fn ensure_success(status: ExitStatus, phase: LifecyclePhase, source: &str) -> Result<()> {
    if status.success() {
        return Ok(());
    }
    bail!("Zed {phase} hook {source} failed with {status}")
}

fn env_truthy(name: &str) -> bool {
    env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn conventions_are_deterministic_across_both_supported_roots() {
        let project = tempfile::tempdir().unwrap();
        write(&project.path().join(".zed/pre-build"), "exit 0\n");
        write(&project.path().join(".zpkg/hooks/pre-build.sh"), "exit 0\n");

        let hooks = resolve_hooks(project.path(), LifecyclePhase::PreBuild).unwrap();
        assert_eq!(
            hooks,
            vec![
                HookSource::Convention(PathBuf::from(".zed/pre-build")),
                HookSource::Convention(PathBuf::from(".zpkg/hooks/pre-build.sh")),
            ]
        );
    }

    #[test]
    fn manifest_can_prepend_supplement_or_replace_conventions() {
        let project = tempfile::tempdir().unwrap();
        write(&project.path().join(".zed/pre-build"), "exit 0\n");
        write(
            &project.path().join(MANIFEST_FILE),
            "[lifecycle.pre-build]\nmode = \"prepend\"\ncommands = [\"first\"]\n",
        );
        let hooks = resolve_hooks(project.path(), LifecyclePhase::PreBuild).unwrap();
        assert!(matches!(hooks[0], HookSource::Manifest { .. }));
        assert!(matches!(hooks[1], HookSource::Convention(_)));

        write(
            &project.path().join(MANIFEST_FILE),
            "[lifecycle.pre-build]\nmode = \"override\"\ncommands = [\"only\"]\n",
        );
        let hooks = resolve_hooks(project.path(), LifecyclePhase::PreBuild).unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(matches!(hooks[0], HookSource::Manifest { .. }));

        write(
            &project.path().join(MANIFEST_FILE),
            "[lifecycle.pre-build]\nmode = \"supplement\"\ncommands = [\"last\"]\n",
        );
        let hooks = resolve_hooks(project.path(), LifecyclePhase::PreBuild).unwrap();
        assert!(matches!(hooks[0], HookSource::Convention(_)));
        assert!(matches!(hooks[1], HookSource::Manifest { .. }));
    }

    #[test]
    fn disable_rejects_ambiguous_commands() {
        let project = tempfile::tempdir().unwrap();
        write(
            &project.path().join(MANIFEST_FILE),
            "[lifecycle.pre-publish]\nmode = \"disable\"\ncommands = [\"should-not-run\"]\n",
        );
        let error = resolve_hooks(project.path(), LifecyclePhase::PrePublish).unwrap_err();
        assert!(error.to_string().contains("mode=disable"));
    }

    #[test]
    fn a_failing_pre_hook_stops_the_operation() {
        let project = tempfile::tempdir().unwrap();
        write(&project.path().join(".zed/pre-build"), "exit 17\n");
        let mut called = false;
        let result = around(
            project.path(),
            LifecyclePhase::PreBuild,
            LifecyclePhase::PostBuild,
            || {
                called = true;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!called);
    }

    #[test]
    fn post_hook_receives_phase_environment() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("phase.txt");
        write(
            &project.path().join(".zpkg/post-pack.sh"),
            "printf '%s' \"$ZED_LIFECYCLE_PHASE\" > phase.txt\n",
        );
        run_phase(project.path(), LifecyclePhase::PostPack).unwrap();
        assert_eq!(fs::read_to_string(output).unwrap(), "post-pack");
    }

    #[cfg(windows)]
    #[test]
    fn windows_shell_paths_strip_verbatim_prefixes() {
        let drive = Path::new(r"\\?\C:\work tree\.zpkg\pre-build.sh");
        assert_eq!(
            windows_native_path(drive),
            PathBuf::from(r"C:\work tree\.zpkg\pre-build.sh")
        );
        assert_eq!(windows_posix_path(drive), "C:/work tree/.zpkg/pre-build.sh");

        let unc = Path::new(r"\\?\UNC\server\share\.zpkg\pre-build.cmd");
        assert_eq!(
            windows_native_path(unc),
            PathBuf::from(r"\\server\share\.zpkg\pre-build.cmd")
        );
    }

    #[test]
    fn misspelled_lifecycle_phase_is_rejected() {
        let project = tempfile::tempdir().unwrap();
        write(
            &project.path().join(MANIFEST_FILE),
            "[lifecycle.pre-buid]\ncommand = \"must-not-run\"\n",
        );
        let error = resolve_hooks(project.path(), LifecyclePhase::PreBuild).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown lifecycle phase `pre-buid`")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_hook_is_rejected() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let target = project.path().join("real-pre-build");
        write(&target, "exit 0\n");
        fs::create_dir_all(project.path().join(".zpkg")).unwrap();
        symlink(&target, project.path().join(".zpkg/pre-build")).unwrap();

        let error = resolve_hooks(project.path(), LifecyclePhase::PreBuild).unwrap_err();
        assert!(error.to_string().contains("must not be a symbolic link"));
    }
}
