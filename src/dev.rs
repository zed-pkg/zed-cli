//! Project-local development shells for humans, coding agents, and CI.
//!
//! `zed develop` composes existing Zed package installation with a managed
//! cross-language environment. Mutable language caches and optional HOME/XDG
//! state live below `.zed/dev`, while declared Zed packages remain resolved by
//! the normal manifest/lockfile installer.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use flags2env::BundledFlags2Env;
use walkdir::{DirEntry, WalkDir};
use zed_interfaces::paths::{BIN_DIR, LOCKFILE_FILE, MANIFEST_FILE, MODULES_DIR};

use crate::cli::{Adapter, Globals, InstallMode};
use crate::config::{Config, read_manifest};
use crate::{manifestless, ops};

const DEFAULT_VENV: &str = ".zed/dev/python/venv";
const NIX_REENTRY_ENV: &str = "ZED_DEV_NIX_ACTIVE";
const MISE_REENTRY_ENV: &str = "ZED_DEV_MISE_ACTIVE";
const DEV_CONTRACT: &str = include_str!("../.dev-cli-flags.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DevNixMode {
    /// Compose with a nearby flake when Nix is available; otherwise continue.
    Auto,
    /// Never re-enter through `nix develop`.
    Never,
    /// Require a nearby flake and a working `nix` executable.
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DevMiseMode {
    /// Compose with project-local mise configuration when mise is available.
    Auto,
    /// Never re-enter through `mise exec`.
    Never,
    /// Require project-local mise configuration and a working `mise` executable.
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DevProfile {
    /// Normal project development tools.
    Default,
    /// Opt-in AI coding tools and `.zed/dev/profiles/ai/bin`.
    Ai,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PythonVenvMode {
    /// Reuse `.venv`, or create a managed venv for detected Python projects.
    Auto,
    /// Do not activate or create a Python virtual environment.
    Never,
    /// Require a usable Python virtual environment, creating it when absent.
    Required,
}

#[derive(Debug, Clone, Args)]
pub struct DevelopArgs {
    /// Run one command through the selected shell instead of entering it.
    #[arg(short = 'c', long, env = "ZED_DEV_COMMAND", value_name = "COMMAND")]
    pub command: Option<String>,

    /// Shell executable; defaults to $SHELL, $COMSPEC, or the platform shell.
    #[arg(long, env = "ZED_DEV_SHELL", value_name = "PATH")]
    pub shell: Option<PathBuf>,

    /// Whether to compose with a nearby `.nix/flake.nix` or `flake.nix`.
    #[arg(long, value_enum, env = "ZED_DEV_NIX", default_value = "auto")]
    pub nix: DevNixMode,

    /// Whether to compose with project-local `mise.toml` or `.mise.toml`.
    #[arg(long, value_enum, env = "ZED_DEV_MISE", default_value = "auto")]
    pub mise: DevMiseMode,

    /// Development profile; `ai` adds the opt-in AI tool shim directory.
    #[arg(long, value_enum, env = "ZED_DEV_PROFILE", default_value = "default")]
    pub profile: DevProfile,

    /// Do not restore packages from `.zpkg.toml` or `.zpkg.lock` first.
    #[arg(long, env = "ZED_DEV_NO_INSTALL")]
    pub no_install: bool,

    /// Require manifest-backed installs to match `.zpkg.lock` exactly.
    #[arg(long, env = "ZED_PKG_FROZEN")]
    pub frozen: bool,

    /// Permit explicitly declared dependency build hooks during restoration.
    #[arg(long, env = "ZED_PKG_ALLOW_BUILD")]
    pub allow_build: bool,

    /// Permit zed to install host-native prerequisites declared by packages.
    #[arg(long, env = "ZED_PKG_ALLOW_NATIVE_DEPS")]
    pub allow_native_deps: bool,

    /// Permit package-authored pre-install and post-install hooks.
    #[arg(long, env = "ZED_PKG_ALLOW_INSTALL_HOOKS")]
    pub allow_install_hooks: bool,

    /// Pin the graph-wide host package manager used during restoration.
    #[arg(long, env = "ZED_PKG_NATIVE_MANAGER", value_name = "MANAGER")]
    pub native_manager: Option<String>,

    /// Redirect HOME and XDG config/data into `.zed/dev` as well as caches.
    #[arg(long, env = "ZED_DEV_ISOLATED_HOME")]
    pub isolated_home: bool,

    /// Print the managed environment as JSON instead of starting a shell.
    #[arg(long, env = "ZED_DEV_PRINT_ENV", conflicts_with = "command")]
    pub print_env: bool,

    /// Python virtual-environment policy.
    #[arg(long, value_enum, env = "ZED_DEV_PYTHON_VENV", default_value = "auto")]
    pub python_venv: PythonVenvMode,

    /// Python interpreter used to create the managed virtual environment.
    #[arg(long, env = "ZED_DEV_PYTHON", value_name = "PATH")]
    pub python: Option<PathBuf>,

    /// Virtual-environment path, relative to the project unless absolute.
    #[arg(
        long,
        env = "ZED_DEV_VENV",
        default_value = DEFAULT_VENV,
        value_name = "PATH"
    )]
    pub venv: PathBuf,
}

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
struct DevelopCli {
    #[command(flatten)]
    globals: Globals,

    #[command(subcommand)]
    command: DevelopCommand,
}

#[derive(Debug, Subcommand)]
enum DevelopCommand {
    /// Enter a package-aware, cross-language virtual development environment.
    #[command(name = "develop", visible_alias = "dev")]
    Develop(DevelopArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Develop,
    DevelopHelp {
        help_index: usize,
        target_index: usize,
    },
    RootHelp,
    Existing,
}

/// Route only the modular `develop` command and augmented root help here. All
/// established commands continue through the repository's existing `Cli` enum.
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    match route(&args) {
        Route::Develop => Some(run_cli(args)),
        Route::DevelopHelp {
            help_index,
            target_index,
        } => {
            let mut rewritten = args;
            rewritten[help_index] = OsString::from("develop");
            rewritten.remove(target_index);
            rewritten.push(OsString::from("--help"));
            Some(run_cli(rewritten))
        }
        Route::RootHelp => Some(print_root_help().map(|()| 0)),
        Route::Existing => None,
    }
}

/// Add the modular command to top-level help and generated shell completions.
pub fn augment_root_command(command: clap::Command) -> clap::Command {
    if command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "develop")
    {
        return command;
    }

    let develop = <DevelopArgs as Args>::augment_args(
        clap::Command::new("develop")
            .visible_alias("dev")
            .about("Enter a package-aware, cross-language virtual development environment"),
    );
    command.subcommand(develop)
}

fn print_root_help() -> Result<()> {
    let mut command = augment_root_command(crate::cli::Cli::command());
    command.print_help().context("printing zed help")?;
    println!();
    Ok(())
}

fn run_cli(args: Vec<OsString>) -> Result<i32> {
    let string_args = utf8_args(&args)?;
    normalize_boolean_environment()?;
    validate_dev_flags(&string_args)?;

    let cli = match DevelopCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error
                .print()
                .context("printing zed develop argument error")?;
            return Ok(code);
        }
    };

    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = env::current_dir().context("reading the current directory")?;
    match cli.command {
        DevelopCommand::Develop(args) => run(&cwd, &cfg, args),
    }
}

/// Enter a package-aware development shell, or run one shell command.
///
/// Returns the child process exit code so CI and agents observe command
/// failures exactly.
pub fn run(requested_root: &Path, cfg: &Config, options: DevelopArgs) -> Result<i32> {
    let root = project_root(requested_root);

    if let Some(code) = maybe_reenter_through_nix(&root, cfg, &options)? {
        return Ok(code);
    }
    if let Some(code) = maybe_reenter_through_mise(&root, cfg, &options)? {
        return Ok(code);
    }

    prepare_directories(&root, options.isolated_home)?;
    if !options.no_install {
        install_declared_tools(&root, cfg, &options)?;
    }
    prepare_cargo_adapter(&root)?;

    let venv = ensure_python_venv(&root, &options)?;
    let environment = managed_environment(&root, &options, venv.as_deref())?;

    if options.print_env {
        print_environment(&environment)?;
        return Ok(0);
    }

    if options.profile == DevProfile::Ai {
        report_ai_profile(&environment, options.isolated_home);
    }

    if options.command.is_none() && (!io::stdin().is_terminal() || !io::stdout().is_terminal()) {
        bail!(
            "`zed develop` needs a real terminal for an interactive shell; use `zed develop -c <command>` for agents and CI"
        );
    }

    spawn_shell(&root, &options, &environment)
}

fn route(args: &[OsString]) -> Route {
    let help_requested = args
        .iter()
        .skip(1)
        .any(|value| value == OsStr::new("--help") || value == OsStr::new("-h"));
    let Some((command_index, command)) = first_command(args) else {
        return if help_requested {
            Route::RootHelp
        } else {
            Route::Existing
        };
    };

    match command.as_str() {
        "develop" | "dev" => Route::Develop,
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target)) if target == "develop" || target == "dev" => {
                Route::DevelopHelp {
                    help_index: command_index,
                    target_index,
                }
            }
            None => Route::RootHelp,
            _ => Route::Existing,
        },
        _ => Route::Existing,
    }
}

fn first_command(args: &[OsString]) -> Option<(usize, String)> {
    let mut index = 1;
    while index < args.len() {
        let token = args[index].to_string_lossy();
        if token == "--" {
            return next_positional(args, index + 1);
        }
        if global_option_takes_value(&token) {
            index += if token.contains('=') { 1 } else { 2 };
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((index, token.into_owned()));
    }
    None
}

fn next_positional(args: &[OsString], mut index: usize) -> Option<(usize, String)> {
    while index < args.len() {
        let token = args[index].to_string_lossy();
        if !token.starts_with('-') {
            return Some((index, token.into_owned()));
        }
        index += 1;
    }
    None
}

fn global_option_takes_value(token: &str) -> bool {
    const OPTIONS: &[&str] = &[
        "--registry",
        "--home",
        "--token",
        "--auth-url",
        "--supabase-url",
        "--supabase-key",
    ];
    OPTIONS.iter().any(|option| {
        token == *option
            || token
                .strip_prefix(option)
                .is_some_and(|remainder| remainder.starts_with('='))
    })
}

fn utf8_args(args: &[OsString]) -> Result<Vec<String>> {
    args.iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .context("flags-2-env requires UTF-8 command-line arguments")
        })
        .collect()
}

fn validate_dev_flags(argv: &[String]) -> Result<()> {
    let parser_argv: Vec<String> = argv
        .iter()
        .filter(|token| !matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
        .cloned()
        .collect();
    let parsed = parse_embedded(&parser_argv)?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "flags2env rejected unknown zed develop option(s): {}",
            parsed
                .unknown_options
                .iter()
                .map(|value| redact_option_value(value))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !parsed.errors.is_empty() {
        bail!(
            "flags2env rejected invalid zed develop value(s): {}",
            parsed
                .errors
                .iter()
                .map(|value| redact_option_value(value))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(())
}

fn parse_embedded(argv: &[String]) -> Result<flags2env::StructuredParse> {
    let contract_dir = tempfile::tempdir().context("creating zed develop flags2env directory")?;
    let contract_path = contract_dir.path().join(".cli-flags.toml");
    fs::write(&contract_path, DEV_CONTRACT).context("writing embedded zed develop contract")?;
    let contract_path = contract_path
        .to_str()
        .context("embedded zed develop contract path is not valid UTF-8")?;

    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed develop flags2env audit failed: {error}"))?;
    parser
        .parse_structured(argv, Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed develop flags2env parse failed: {error}"))
}

fn normalize_boolean_environment() -> Result<()> {
    const BOOLEAN_ENVIRONMENTS: &[&str] = &[
        "ZED_DEV_NO_INSTALL",
        "ZED_PKG_FROZEN",
        "ZED_PKG_ALLOW_BUILD",
        "ZED_PKG_ALLOW_NATIVE_DEPS",
        "ZED_PKG_ALLOW_INSTALL_HOOKS",
        "ZED_PKG_INTERACTIVE",
        "ZED_DEV_ISOLATED_HOME",
        "ZED_DEV_PRINT_ENV",
    ];
    for key in BOOLEAN_ENVIRONMENTS {
        let Some(raw) = env::var_os(key) else {
            continue;
        };
        let raw = raw
            .to_str()
            .with_context(|| format!("boolean environment variable `{key}` is not UTF-8"))?;
        let normalized = match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => "true",
            "false" | "0" | "no" | "off" => "false",
            _ => bail!(
                "boolean environment variable `{key}` must be true/false, 1/0, yes/no, or on/off"
            ),
        };
        if raw != normalized {
            // SAFETY: this runs once at process startup, before typed parsing
            // or any worker threads read this command's environment.
            unsafe { env::set_var(key, normalized) };
        }
    }
    Ok(())
}

fn redact_option_value(value: &str) -> String {
    match value.split_once('=') {
        Some((option, _)) if option.starts_with('-') => format!("{option}=<redacted>"),
        _ => value.to_string(),
    }
}

/// Match manifestless root selection for nested invocations without guessing
/// in ambiguous monorepos.
fn project_root(requested: &Path) -> PathBuf {
    let requested = fs::canonicalize(requested).unwrap_or_else(|_| requested.to_path_buf());

    if let Some(root) = requested
        .ancestors()
        .find(|candidate| candidate.join(MANIFEST_FILE).is_file())
    {
        return root.to_path_buf();
    }
    if let Some(root) = requested.ancestors().find(|candidate| {
        candidate.join(LOCKFILE_FILE).is_file()
            || ops::detect_native_manifest_target(candidate).is_some()
    }) {
        return root.to_path_buf();
    }
    if let Some(root) = requested
        .ancestors()
        .find(|candidate| ops::detect_structure_target(candidate).is_some())
    {
        return root.to_path_buf();
    }

    unique_nested_project(&requested).unwrap_or(requested)
}

fn unique_nested_project(root: &Path) -> Option<PathBuf> {
    let mut candidates = BTreeSet::new();
    for entry in WalkDir::new(root)
        .min_depth(1)
        .max_depth(4)
        .into_iter()
        .filter_entry(include_project_entry)
        .flatten()
        .filter(|entry| entry.file_type().is_dir())
    {
        let candidate = entry.path();
        if candidate.join(MANIFEST_FILE).is_file()
            || candidate.join(LOCKFILE_FILE).is_file()
            || ops::detect_native_manifest_target(candidate).is_some()
        {
            candidates.insert(candidate.to_path_buf());
        }
    }
    let mut candidates = candidates.into_iter();
    match (candidates.next(), candidates.next()) {
        (Some(candidate), None) => Some(candidate),
        _ => None,
    }
}

fn include_project_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    !matches!(
        name.as_ref(),
        ".git" | ".hg" | ".jj" | ".zed" | "node_modules" | "target" | "zed_modules"
    )
}

fn maybe_reenter_through_nix(
    root: &Path,
    cfg: &Config,
    options: &DevelopArgs,
) -> Result<Option<i32>> {
    if options.nix == DevNixMode::Never
        || env::var_os(NIX_REENTRY_ENV).is_some()
        || env::var_os("IN_NIX_SHELL").is_some()
    {
        return Ok(None);
    }

    let Some(flake) = nix_flake(root) else {
        if options.nix == DevNixMode::Required {
            bail!(
                "--nix required was requested, but no `.nix/flake.nix` or `flake.nix` was found at or above {}",
                root.display()
            );
        }
        return Ok(None);
    };

    if !program_available("nix") {
        if options.nix == DevNixMode::Required {
            bail!("--nix required was requested, but `nix` is not available on PATH");
        }
        eprintln!(
            "zed develop: found {}, but `nix` is unavailable; continuing with the native Zed environment",
            flake.display()
        );
        return Ok(None);
    }

    let executable = env::current_exe().context("locating the running zed executable")?;
    let mut command = Command::new("nix");
    command
        .arg("develop")
        .arg(&flake)
        .arg("--command")
        .arg(executable)
        .arg("develop")
        .arg("--nix")
        .arg("never")
        .arg("--mise")
        .arg(mise_mode_name(options.mise))
        .arg("--profile")
        .arg(profile_name(options.profile))
        .arg("--python-venv")
        .arg(python_venv_mode_name(options.python_venv))
        .arg("--venv")
        .arg(&options.venv)
        .env(NIX_REENTRY_ENV, "1")
        .env("ZED_PKG_REGISTRY", &cfg.registry)
        .env("ZED_PKG_HOME", &cfg.home)
        .env("ZED_PKG_AUTH_URL", &cfg.auth_url)
        .current_dir(root);

    if let Some(token) = &cfg.token {
        command.env("ZED_PKG_TOKEN", token);
    }
    if let Some(url) = &cfg.supabase_url {
        command.env("ZED_PKG_SUPABASE_URL", url);
    }
    if let Some(key) = &cfg.supabase_key {
        command.env("ZED_PKG_SUPABASE_KEY", key);
    }

    if let Some(shell) = &options.shell {
        command.arg("--shell").arg(shell);
    }
    if let Some(python) = &options.python {
        command.arg("--python").arg(python);
    }
    if options.no_install {
        command.arg("--no-install");
    }
    if options.frozen {
        command.arg("--frozen");
    }
    if options.allow_build {
        command.arg("--allow-build");
    }
    if options.allow_native_deps {
        command.arg("--allow-native-deps");
    }
    if options.allow_install_hooks {
        command.arg("--allow-install-hooks");
    }
    if let Some(manager) = &options.native_manager {
        command.arg("--native-manager").arg(manager);
    }
    if options.isolated_home {
        command.arg("--isolated-home");
    }
    if options.print_env {
        command.arg("--print-env");
    }
    if let Some(script) = &options.command {
        command.arg("-c").arg(script);
    }

    let status = command
        .status()
        .with_context(|| format!("entering Nix development shell at {}", flake.display()))?;
    Ok(Some(status.code().unwrap_or(1)))
}

fn nix_flake(root: &Path) -> Option<PathBuf> {
    for candidate in root.ancestors() {
        let nested = candidate.join(".nix/flake.nix");
        if nested.is_file() {
            return Some(candidate.join(".nix"));
        }
        if candidate.join("flake.nix").is_file() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn maybe_reenter_through_mise(
    root: &Path,
    cfg: &Config,
    options: &DevelopArgs,
) -> Result<Option<i32>> {
    if options.mise == DevMiseMode::Never || env::var_os(MISE_REENTRY_ENV).is_some() {
        return Ok(None);
    }

    let Some(config) = mise_config(root) else {
        if options.mise == DevMiseMode::Required {
            bail!(
                "--mise required was requested, but no project-local `mise.toml` or `.mise.toml` was found inside the owning checkout for {}",
                root.display()
            );
        }
        return Ok(None);
    };

    if env::var_os("__MISE_DIFF").is_some() {
        if options.frozen {
            bail!(
                "--frozen mise composition cannot verify an ambient mise activation; run `zed dev --mise required --frozen` outside an activated mise shell, or use `--mise never` after an explicit `mise exec`"
            );
        }
        return Ok(None);
    }

    if !program_available("mise") {
        if options.mise == DevMiseMode::Required {
            bail!("--mise required was requested, but `mise` is not available on PATH");
        }
        eprintln!(
            "zed develop: found {}, but `mise` is unavailable; continuing with the native Zed environment",
            config.display()
        );
        return Ok(None);
    }

    let lockfile = mise_lockfile(&config);
    if options.frozen && !lockfile.is_file() {
        bail!(
            "--frozen with {} requires the adjacent mise lockfile {}",
            config.display(),
            lockfile.display()
        );
    }

    let executable = env::current_exe().context("locating the running zed executable")?;
    let mut command = Command::new("mise");
    command
        .arg("exec")
        .arg("--")
        .arg(executable)
        .arg("develop")
        .arg("--nix")
        .arg("never")
        .arg("--mise")
        .arg("never")
        .arg("--profile")
        .arg(profile_name(options.profile))
        .arg("--python-venv")
        .arg(python_venv_mode_name(options.python_venv))
        .arg("--venv")
        .arg(&options.venv)
        .env(MISE_REENTRY_ENV, "1")
        .env("ZED_PKG_REGISTRY", &cfg.registry)
        .env("ZED_PKG_HOME", &cfg.home)
        .env("ZED_PKG_AUTH_URL", &cfg.auth_url)
        .current_dir(root);

    let boundary = mise_project_boundary(root);
    if let Some(ceiling) = boundary.parent() {
        command.env("MISE_CEILING_PATHS", ceiling);
    }

    if options.frozen {
        let config_dir = root.join(".zed/dev/mise/config");
        let system_config_dir = root.join(".zed/dev/mise/system-config");
        fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating isolated mise config {}", config_dir.display()))?;
        fs::create_dir_all(&system_config_dir).with_context(|| {
            format!(
                "creating isolated mise system config {}",
                system_config_dir.display()
            )
        })?;
        let config_filename = config
            .file_name()
            .context("mise configuration path has no file name")?;
        command
            .env("MISE_LOCKED", "1")
            .env("MISE_CONFIG_DIR", config_dir)
            .env("MISE_SYSTEM_CONFIG_DIR", system_config_dir)
            .env("MISE_OVERRIDE_CONFIG_FILENAMES", config_filename)
            .env("MISE_OVERRIDE_TOOL_VERSIONS_FILENAMES", "none");
    }

    if let Some(token) = &cfg.token {
        command.env("ZED_PKG_TOKEN", token);
    }
    if let Some(url) = &cfg.supabase_url {
        command.env("ZED_PKG_SUPABASE_URL", url);
    }
    if let Some(key) = &cfg.supabase_key {
        command.env("ZED_PKG_SUPABASE_KEY", key);
    }

    if let Some(shell) = &options.shell {
        command.arg("--shell").arg(shell);
    }
    if let Some(python) = &options.python {
        command.arg("--python").arg(python);
    }
    if options.no_install {
        command.arg("--no-install");
    }
    if options.frozen {
        command.arg("--frozen");
    }
    if options.allow_build {
        command.arg("--allow-build");
    }
    if options.isolated_home {
        command.arg("--isolated-home");
    }
    if options.print_env {
        command.arg("--print-env");
    }
    if let Some(script) = &options.command {
        command.arg("-c").arg(script);
    }

    let status = command.status().with_context(|| {
        format!(
            "entering mise development environment from {}",
            config.display()
        )
    })?;
    Ok(Some(status.code().unwrap_or(1)))
}

fn mise_config(root: &Path) -> Option<PathBuf> {
    let boundary = mise_project_boundary(root);
    for candidate in root.ancestors() {
        for filename in ["mise.toml", ".mise.toml"] {
            let config = candidate.join(filename);
            if config.is_file() {
                return Some(config);
            }
        }
        if candidate == boundary {
            break;
        }
    }
    None
}

fn mise_project_boundary(root: &Path) -> PathBuf {
    root.ancestors()
        .find(|candidate| {
            [".git", ".hg", ".jj"]
                .iter()
                .any(|marker| candidate.join(marker).exists())
        })
        .unwrap_or(root)
        .to_path_buf()
}

fn mise_lockfile(config: &Path) -> PathBuf {
    let directory = config
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    directory.join("mise.lock")
}

fn mise_mode_name(mode: DevMiseMode) -> &'static str {
    match mode {
        DevMiseMode::Auto => "auto",
        DevMiseMode::Never => "never",
        DevMiseMode::Required => "required",
    }
}

fn profile_name(profile: DevProfile) -> &'static str {
    match profile {
        DevProfile::Default => "default",
        DevProfile::Ai => "ai",
    }
}

fn python_venv_mode_name(mode: PythonVenvMode) -> &'static str {
    match mode {
        PythonVenvMode::Auto => "auto",
        PythonVenvMode::Never => "never",
        PythonVenvMode::Required => "required",
    }
}

fn program_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn prepare_directories(root: &Path, isolated_home: bool) -> Result<()> {
    let dev = root.join(".zed/dev");
    let mut directories = vec![
        dev.join("bin"),
        dev.join("cargo/home/bin"),
        dev.join("cargo/target"),
        dev.join("go/bin"),
        dev.join("go/cache"),
        dev.join("go/pkg/mod"),
        dev.join("go/path"),
        dev.join("node/cache"),
        dev.join("node/corepack"),
        dev.join("node/pnpm"),
        dev.join("node/prefix/bin"),
        dev.join("python/cache/pip"),
        dev.join("python/cache/uv"),
        dev.join("dart/pub-cache"),
        dev.join("java/gradle"),
        dev.join("ruby/gems/bin"),
        dev.join("xdg/cache"),
        dev.join("xdg/state"),
        dev.join("profiles/ai/bin"),
    ];
    if isolated_home {
        directories.extend([
            dev.join("home"),
            dev.join("xdg/config"),
            dev.join("xdg/data"),
        ]);
    }
    for directory in directories {
        fs::create_dir_all(&directory)
            .with_context(|| format!("creating development directory {}", directory.display()))?;
    }
    Ok(())
}

fn install_declared_tools(root: &Path, cfg: &Config, options: &DevelopArgs) -> Result<()> {
    if root.join(MANIFEST_FILE).is_file() {
        let permissions = ops::InstallPermissions {
            allow_build: options.allow_build,
            allow_native_deps: options.allow_native_deps,
            allow_install_hooks: options.allow_install_hooks,
            native_manager: options.native_manager.clone(),
        };
        ops::install_with_permissions(
            root,
            cfg,
            options.frozen,
            InstallMode::Symlink,
            Adapter::Auto,
            &permissions,
            None,
            false,
        )?;
        return Ok(());
    }

    if root.join(LOCKFILE_FILE).is_file() {
        manifestless::install(
            root,
            cfg,
            &[],
            true,
            InstallMode::Symlink,
            Adapter::Auto,
            options.allow_build,
            options.allow_native_deps,
            options.allow_install_hooks,
            options.native_manager.as_deref(),
            None,
            true,
            false,
        )?;
        return Ok(());
    }

    if options.frozen {
        bail!(
            "--frozen requires {MANIFEST_FILE} or {LOCKFILE_FILE} in the selected project {}",
            root.display()
        );
    }

    eprintln!(
        "zed develop: no {MANIFEST_FILE} or {LOCKFILE_FILE} found; starting a language-only environment without installing Zed packages"
    );
    Ok(())
}

fn prepare_cargo_adapter(root: &Path) -> Result<()> {
    let source = root.join(".zed/cargo-paths.toml");
    if !source.is_file() {
        return Ok(());
    }
    let destination = root.join(".zed/dev/cargo/home/config.toml");
    fs::copy(&source, &destination).with_context(|| {
        format!(
            "copying Rust adapter {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn ensure_python_venv(root: &Path, options: &DevelopArgs) -> Result<Option<PathBuf>> {
    if options.python_venv == PythonVenvMode::Never {
        return Ok(None);
    }

    let configured = resolve_venv_path(root, &options.venv);
    let conventional = root.join(".venv");
    let venv = if options.venv.as_path() == Path::new(DEFAULT_VENV) && conventional.is_dir() {
        conventional
    } else {
        configured
    };

    if venv.is_dir() {
        validate_venv(&venv)?;
        return Ok(Some(venv));
    }

    let is_python_project = ops::detect_target(root).as_deref() == Some("python");
    if options.python_venv == PythonVenvMode::Auto && !is_python_project {
        return Ok(None);
    }

    if let Some(parent) = venv.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating Python venv parent {}", parent.display()))?;
    }

    let explicit_python = options.python.is_some();
    let candidates: Vec<PathBuf> = options
        .python
        .clone()
        .map(|path| vec![path])
        .unwrap_or_else(|| vec![PathBuf::from("python3"), PathBuf::from("python")]);
    let mut failures = Vec::new();

    for candidate in candidates {
        match Command::new(&candidate)
            .args(["-m", "venv"])
            .arg(&venv)
            .current_dir(root)
            .status()
        {
            Ok(status) if status.success() => {
                validate_venv(&venv)?;
                eprintln!(
                    "zed develop: created project-local Python environment at {}",
                    venv.display()
                );
                return Ok(Some(venv));
            }
            Ok(status) => failures.push(format!(
                "{} -m venv exited with status {status}",
                candidate.display()
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                failures.push(format!("{} was not found", candidate.display()));
            }
            Err(error) => failures.push(format!("{}: {error}", candidate.display())),
        }
        if explicit_python {
            break;
        }
    }

    let detail = failures.join("; ");
    if options.python_venv == PythonVenvMode::Auto {
        eprintln!(
            "zed develop: Python project detected, but no venv could be created ({detail}); continuing without one (use --python-venv required to fail closed)"
        );
        Ok(None)
    } else {
        bail!(
            "--python-venv required could not create {}: {detail}; pass --python <path> or install Python venv support",
            venv.display()
        )
    }
}

fn resolve_venv_path(root: &Path, configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        root.join(configured)
    }
}

fn validate_venv(venv: &Path) -> Result<()> {
    let bin = venv_bin_dir(venv);
    if !bin.is_dir() {
        bail!(
            "{} exists but is not a usable Python virtual environment (missing {})",
            venv.display(),
            bin.display()
        );
    }
    Ok(())
}

fn venv_bin_dir(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts")
    } else {
        venv.join("bin")
    }
}

fn managed_environment(
    root: &Path,
    options: &DevelopArgs,
    venv: Option<&Path>,
) -> Result<BTreeMap<String, OsString>> {
    let dev = root.join(".zed/dev");
    let modules = read_manifest(root)
        .map(|manifest| manifest.modules_dir().to_string())
        .unwrap_or_else(|_| MODULES_DIR.to_string());

    let mut paths = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(venv) = venv {
        push_path(&mut paths, &mut seen, venv_bin_dir(venv));
    }
    push_path(&mut paths, &mut seen, root.join(&modules).join(BIN_DIR));
    push_path(&mut paths, &mut seen, root.join("node_modules/.bin"));
    if options.profile == DevProfile::Ai {
        push_path(&mut paths, &mut seen, dev.join("profiles/ai/bin"));
    }
    push_path(&mut paths, &mut seen, dev.join("bin"));
    push_path(&mut paths, &mut seen, dev.join("node/pnpm"));
    push_path(&mut paths, &mut seen, dev.join("node/prefix/bin"));
    push_path(&mut paths, &mut seen, dev.join("cargo/home/bin"));
    push_path(&mut paths, &mut seen, dev.join("go/bin"));
    push_path(&mut paths, &mut seen, dev.join("ruby/gems/bin"));
    if let Some(existing) = env::var_os("PATH") {
        for path in env::split_paths(&existing) {
            push_path(&mut paths, &mut seen, path);
        }
    }
    let path = env::join_paths(paths).context("assembling PATH for zed develop")?;

    let mut values = BTreeMap::new();
    insert_value(&mut values, "PATH", path);
    insert_value(&mut values, "ZED_DEV", "1");
    insert_value(&mut values, "ZED_DEV_ROOT", dev.as_os_str());
    insert_value(&mut values, "ZED_DEV_PROJECT_ROOT", root.as_os_str());
    insert_value(
        &mut values,
        "ZED_DEV_PROFILE",
        profile_name(options.profile),
    );

    insert_value(&mut values, "CARGO_HOME", dev.join("cargo/home"));
    insert_value(&mut values, "CARGO_TARGET_DIR", dev.join("cargo/target"));
    insert_value(&mut values, "GOBIN", dev.join("go/bin"));
    insert_value(&mut values, "GOPATH", dev.join("go/path"));
    insert_value(&mut values, "GOMODCACHE", dev.join("go/pkg/mod"));
    insert_value(&mut values, "GOCACHE", dev.join("go/cache"));
    insert_value(&mut values, "COREPACK_HOME", dev.join("node/corepack"));
    insert_value(&mut values, "npm_config_cache", dev.join("node/cache/npm"));
    insert_value(&mut values, "npm_config_prefix", dev.join("node/prefix"));
    insert_value(
        &mut values,
        "YARN_CACHE_FOLDER",
        dev.join("node/cache/yarn"),
    );
    insert_value(&mut values, "PNPM_HOME", dev.join("node/pnpm"));
    insert_value(&mut values, "PIP_CACHE_DIR", dev.join("python/cache/pip"));
    insert_value(&mut values, "UV_CACHE_DIR", dev.join("python/cache/uv"));
    insert_value(&mut values, "PYTHONNOUSERSITE", "1");
    insert_value(&mut values, "PUB_CACHE", dev.join("dart/pub-cache"));
    insert_value(&mut values, "GRADLE_USER_HOME", dev.join("java/gradle"));
    insert_value(&mut values, "GEM_HOME", dev.join("ruby/gems"));
    insert_value(&mut values, "GEM_PATH", dev.join("ruby/gems"));
    insert_value(&mut values, "XDG_CACHE_HOME", dev.join("xdg/cache"));
    insert_value(&mut values, "XDG_STATE_HOME", dev.join("xdg/state"));

    if options.isolated_home {
        insert_value(&mut values, "HOME", dev.join("home"));
        if cfg!(windows) {
            insert_value(&mut values, "USERPROFILE", dev.join("home"));
        }
        insert_value(&mut values, "XDG_CONFIG_HOME", dev.join("xdg/config"));
        insert_value(&mut values, "XDG_DATA_HOME", dev.join("xdg/data"));
    }

    if let Some(venv) = venv {
        insert_value(&mut values, "VIRTUAL_ENV", venv);
        insert_value(&mut values, "UV_PROJECT_ENVIRONMENT", venv);
    }

    let pythonpath = root.join(".zed/pythonpath");
    if pythonpath.is_file() {
        let declared = fs::read_to_string(&pythonpath)
            .with_context(|| format!("reading {}", pythonpath.display()))?;
        let mut python_paths: Vec<PathBuf> =
            env::split_paths(OsStr::new(declared.trim())).collect();
        if let Some(existing) = env::var_os("PYTHONPATH") {
            python_paths.extend(env::split_paths(&existing));
        }
        insert_value(
            &mut values,
            "PYTHONPATH",
            env::join_paths(python_paths).context("assembling PYTHONPATH for zed develop")?,
        );
    }

    let go_work = root.join(".zed/go.work");
    if go_work.is_file() {
        insert_value(&mut values, "GOWORK", go_work);
    }

    let classpath = root.join(".zed/classpath");
    if classpath.is_file() {
        let declared = fs::read_to_string(&classpath)
            .with_context(|| format!("reading {}", classpath.display()))?;
        let mut class_paths: Vec<PathBuf> = env::split_paths(OsStr::new(declared.trim())).collect();
        if let Some(existing) = env::var_os("CLASSPATH") {
            class_paths.extend(env::split_paths(&existing));
        }
        insert_value(
            &mut values,
            "CLASSPATH",
            env::join_paths(class_paths).context("assembling CLASSPATH for zed develop")?,
        );
    }

    Ok(values)
}

fn insert_value(
    values: &mut BTreeMap<String, OsString>,
    key: impl Into<String>,
    value: impl AsRef<OsStr>,
) {
    values.insert(key.into(), value.as_ref().to_os_string());
}

fn push_path(paths: &mut Vec<PathBuf>, seen: &mut BTreeSet<OsString>, path: PathBuf) {
    let key = path.as_os_str().to_os_string();
    if seen.insert(key) {
        paths.push(path);
    }
}

fn print_environment(environment: &BTreeMap<String, OsString>) -> Result<()> {
    let printable: BTreeMap<&str, String> = environment
        .iter()
        .map(|(key, value)| (key.as_str(), value.to_string_lossy().into_owned()))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&printable)
            .context("serializing the managed development environment")?
    );
    Ok(())
}

fn report_ai_profile(environment: &BTreeMap<String, OsString>, isolated_home: bool) {
    const TOOLS: &[&str] = &["codex", "claude", "gemini", "kimi", "qwen", "opencode"];
    let path = environment.get("PATH").map(OsString::as_os_str);
    let available: Vec<&str> = TOOLS
        .iter()
        .copied()
        .filter(|tool| executable_on_path(tool, path))
        .collect();

    if available.is_empty() {
        eprintln!(
            "zed develop: AI profile enabled, but no known AI CLI is available; declare a Zed package exposing codex/claude/gemini/kimi/qwen/opencode or place a shim in .zed/dev/profiles/ai/bin"
        );
    } else {
        eprintln!(
            "zed develop: AI profile tools available: {}",
            available.join(", ")
        );
    }
    if isolated_home {
        eprintln!(
            "zed develop: isolated HOME is active; provider credentials are never copied into it, so authenticate there or pass credentials through the process environment"
        );
    }
}

fn executable_on_path(program: &str, path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    for directory in env::split_paths(path) {
        if directory.join(program).is_file() {
            return true;
        }
        if cfg!(windows) && directory.join(format!("{program}.exe")).is_file() {
            return true;
        }
    }
    false
}

fn spawn_shell(
    root: &Path,
    options: &DevelopArgs,
    environment: &BTreeMap<String, OsString>,
) -> Result<i32> {
    let shell = resolve_shell(options.shell.as_deref());
    let mut command = Command::new(&shell);
    configure_shell_arguments(&mut command, &shell, options.command.as_deref());
    let current_dir = child_process_current_dir(root);
    let status = command
        .envs(environment)
        .current_dir(&current_dir)
        .status()
        .with_context(|| format!("starting development shell {}", shell.display()))?;
    Ok(status.code().unwrap_or(1))
}

fn child_process_current_dir(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
        PathBuf::from(OsString::from_wide(&normalize_windows_child_current_dir(
            &wide,
        )))
    }

    #[cfg(not(windows))]
    {
        root.to_path_buf()
    }
}

#[cfg(any(windows, test))]
fn normalize_windows_child_current_dir(wide: &[u16]) -> Vec<u16> {
    const SLASH: u16 = b'\\' as u16;
    const VERBATIM: &[u16] = &[SLASH, SLASH, b'?' as u16, SLASH];
    const VERBATIM_UNC: &[u16] = &[
        SLASH,
        SLASH,
        b'?' as u16,
        SLASH,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        SLASH,
    ];

    if wide.starts_with(VERBATIM_UNC) {
        let mut normalized = Vec::with_capacity(wide.len() - VERBATIM_UNC.len() + 2);
        normalized.extend_from_slice(&[SLASH, SLASH]);
        normalized.extend_from_slice(&wide[VERBATIM_UNC.len()..]);
        normalized
    } else if wide.starts_with(VERBATIM) {
        wide[VERBATIM.len()..].to_vec()
    } else {
        wide.to_vec()
    }
}

fn resolve_shell(explicit: Option<&Path>) -> PathBuf {
    if let Some(shell) = explicit {
        return shell.to_path_buf();
    }
    if let Some(shell) = env::var_os("SHELL").filter(|value| !value.is_empty()) {
        return shell.into();
    }
    if cfg!(windows) {
        if let Some(shell) = env::var_os("COMSPEC").filter(|value| !value.is_empty()) {
            return shell.into();
        }
        PathBuf::from("cmd.exe")
    } else {
        PathBuf::from("/bin/sh")
    }
}

fn configure_shell_arguments(command: &mut Command, shell: &Path, script: Option<&str>) {
    let Some(script) = script else {
        return;
    };
    let name = shell
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match name.as_str() {
        "cmd" | "cmd.exe" => {
            command.args(["/D", "/S", "/C", script]);
        }
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => {
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ]);
        }
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => {
            command.args(["-c", script]);
        }
        _ => {
            command.args(["-c", script]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> DevelopArgs {
        DevelopArgs {
            command: None,
            shell: None,
            nix: DevNixMode::Never,
            mise: DevMiseMode::Never,
            profile: DevProfile::Default,
            no_install: true,
            frozen: false,
            allow_build: false,
            allow_native_deps: false,
            allow_install_hooks: false,
            native_manager: None,
            isolated_home: false,
            print_env: false,
            python_venv: PythonVenvMode::Never,
            python: None,
            venv: PathBuf::from(DEFAULT_VENV),
        }
    }

    #[test]
    fn noninteractive_posix_commands_do_not_use_login_shells() {
        let mut command = Command::new("/bin/bash");
        configure_shell_arguments(&mut command, Path::new("/bin/bash"), Some("true"));
        let arguments: Vec<OsString> = command.get_args().map(OsStr::to_os_string).collect();
        assert_eq!(
            arguments,
            vec![OsString::from("-c"), OsString::from("true")]
        );
    }

    #[test]
    fn powershell_command_mode_disables_profiles_and_interaction() {
        let script = "Write-Output profile-safe";
        for shell in ["pwsh.exe", "powershell.exe"] {
            let mut command = Command::new(shell);
            configure_shell_arguments(&mut command, Path::new(shell), Some(script));
            let arguments: Vec<OsString> = command.get_args().map(OsStr::to_os_string).collect();
            assert_eq!(
                arguments,
                vec![
                    OsString::from("-NoLogo"),
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-Command"),
                    OsString::from(script),
                ],
                "unexpected command-mode arguments for {shell}"
            );
        }
    }

    #[test]
    fn interactive_powershell_retains_native_startup_semantics() {
        let mut command = Command::new("pwsh.exe");
        configure_shell_arguments(&mut command, Path::new("pwsh.exe"), None);
        assert_eq!(command.get_args().count(), 0);
    }

    #[test]
    fn cmd_command_mode_disables_autorun() {
        let script = "echo profile-safe";
        for shell in ["cmd", "cmd.exe"] {
            let mut command = Command::new(shell);
            configure_shell_arguments(&mut command, Path::new(shell), Some(script));
            let arguments: Vec<OsString> = command.get_args().map(OsStr::to_os_string).collect();
            assert_eq!(
                arguments,
                vec![
                    OsString::from("/D"),
                    OsString::from("/S"),
                    OsString::from("/C"),
                    OsString::from(script),
                ],
                "unexpected command-mode arguments for {shell}"
            );
        }
    }

    fn utf16(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    fn from_utf16(value: &[u16]) -> String {
        String::from_utf16(value).expect("valid UTF-16 fixture")
    }

    #[test]
    fn windows_child_cwd_strips_verbatim_disk_prefix_without_losing_unicode() {
        let input = utf16(r"\\?\C:\répo\工具");
        let normalized = normalize_windows_child_current_dir(&input);
        assert_eq!(from_utf16(&normalized), r"C:\répo\工具");
    }

    #[test]
    fn windows_child_cwd_converts_verbatim_unc_to_standard_unc() {
        let input = utf16(r"\\?\UNC\server\share\repo");
        let normalized = normalize_windows_child_current_dir(&input);
        assert_eq!(from_utf16(&normalized), r"\\server\share\repo");
    }

    #[test]
    fn windows_child_cwd_preserves_non_verbatim_paths() {
        for value in [r"C:\repo\nested", r"\\server\share\repo", r"\\.\PIPE\zed"] {
            let input = utf16(value);
            assert_eq!(normalize_windows_child_current_dir(&input), input);
        }
    }

    #[test]
    fn routes_canonical_alias_and_help_spellings() {
        let strings = [
            vec!["zed", "develop", "-c", "true"],
            vec!["zed", "dev", "-c", "true"],
        ];
        for argv in strings {
            let argv: Vec<OsString> = argv.into_iter().map(OsString::from).collect();
            assert_eq!(route(&argv), Route::Develop);
        }

        let help: Vec<OsString> = ["zed", "help", "dev"]
            .into_iter()
            .map(OsString::from)
            .collect();
        assert!(matches!(route(&help), Route::DevelopHelp { .. }));

        let root: Vec<OsString> = ["zed", "--help"].into_iter().map(OsString::from).collect();
        assert_eq!(route(&root), Route::RootHelp);
    }

    #[test]
    fn parser_accepts_develop_and_dev_with_the_same_arguments() {
        for command in ["develop", "dev"] {
            let cli = DevelopCli::try_parse_from([
                "zed",
                command,
                "--nix",
                "never",
                "--mise",
                "required",
                "--profile",
                "ai",
                "-c",
                "cargo test",
            ])
            .unwrap();
            let DevelopCommand::Develop(args) = cli.command;
            assert_eq!(args.nix, DevNixMode::Never);
            assert_eq!(args.mise, DevMiseMode::Required);
            assert_eq!(args.profile, DevProfile::Ai);
            assert_eq!(args.command.as_deref(), Some("cargo test"));
        }
    }

    #[test]
    fn flags2env_contract_audits_and_maps_the_short_command_flag() {
        let argv = vec![
            "zed".to_string(),
            "dev".to_string(),
            "-c".to_string(),
            "cargo test".to_string(),
            "--isolated-home".to_string(),
        ];
        let parsed = parse_embedded(&argv).unwrap();
        assert!(parsed.unknown_options.is_empty());
        assert!(parsed.errors.is_empty());
        assert_eq!(
            parsed.flags.get("ZED_DEV_COMMAND").map(String::as_str),
            Some("cargo test")
        );
        assert_eq!(
            parsed
                .flags
                .get("ZED_DEV_ISOLATED_HOME")
                .map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn every_develop_flag_has_a_flags2env_environment_fallback() {
        let command = <DevelopArgs as Args>::augment_args(clap::Command::new("develop"));
        for argument in command.get_arguments() {
            let Some(long) = argument.get_long() else {
                continue;
            };
            if long == "help" {
                continue;
            }
            let environment = argument
                .get_env()
                .unwrap_or_else(|| panic!("--{long} has no environment fallback"))
                .to_string_lossy();
            assert!(
                environment.starts_with("ZED_DEV_") || environment.starts_with("ZED_PKG_"),
                "unexpected environment key for --{long}: {environment}"
            );
        }
    }

    #[test]
    fn develop_contract_and_typed_parser_expose_the_same_environment_keys() {
        fn collect_command_envs(command: &clap::Command, envs: &mut BTreeSet<String>) {
            for argument in command.get_arguments() {
                let Some(long) = argument.get_long() else {
                    continue;
                };
                if long == "help" || long == "version" {
                    continue;
                }
                let environment = argument
                    .get_env()
                    .unwrap_or_else(|| panic!("--{long} has no environment fallback"));
                envs.insert(environment.to_string_lossy().into_owned());
            }
            for subcommand in command.get_subcommands() {
                collect_command_envs(subcommand, envs);
            }
        }

        fn collect_contract_envs(value: &toml::Value, envs: &mut BTreeSet<String>) {
            let Some(table) = value.as_table() else {
                return;
            };
            if let Some(flags) = table.get("flags").and_then(toml::Value::as_table) {
                for flag in flags.values() {
                    let environment = flag
                        .get("env")
                        .and_then(toml::Value::as_str)
                        .expect("every contract flag must declare env");
                    envs.insert(environment.to_string());
                }
            }
            for child in table.values() {
                collect_contract_envs(child, envs);
            }
        }

        let contract: toml::Value =
            toml::from_str(DEV_CONTRACT).expect("develop contract must be valid TOML");
        let mut contract_envs = BTreeSet::new();
        collect_contract_envs(&contract, &mut contract_envs);

        let mut command_envs = BTreeSet::new();
        collect_command_envs(&DevelopCli::command(), &mut command_envs);
        assert_eq!(contract_envs, command_envs);
    }

    #[test]
    fn augmented_root_help_contains_the_command_and_alias() {
        let command = augment_root_command(crate::cli::Cli::command());
        let develop = command.find_subcommand("develop").unwrap();
        assert!(develop.get_all_aliases().any(|alias| alias == "dev"));
        assert!(
            develop
                .get_arguments()
                .any(|arg| arg.get_long() == Some("nix"))
        );
        assert!(
            develop
                .get_arguments()
                .any(|arg| arg.get_long() == Some("mise"))
        );
    }

    #[test]
    fn nested_invocations_select_the_owning_manifest() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let nested = root.join("apps/web/src");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join(MANIFEST_FILE), "[package]\n").unwrap();
        assert_eq!(project_root(&nested), fs::canonicalize(root).unwrap());
    }

    #[test]
    fn one_clear_nested_native_project_is_selected_but_ambiguous_roots_are_not() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("apps/web");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("package.json"), "{}").unwrap();
        assert_eq!(project_root(temp.path()), fs::canonicalize(app).unwrap());

        let second = temp.path().join("apps/api");
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(
            project_root(temp.path()),
            fs::canonicalize(temp.path()).unwrap()
        );
    }

    #[test]
    fn dot_nix_flake_takes_precedence_and_is_found_above_a_nested_project() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("apps/web");
        fs::create_dir_all(temp.path().join(".nix")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(temp.path().join(".nix/flake.nix"), "{}").unwrap();
        fs::write(temp.path().join("flake.nix"), "{}").unwrap();
        assert_eq!(nix_flake(&nested), Some(temp.path().join(".nix")));
    }

    #[test]
    fn mise_config_stays_inside_the_owning_vcs_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        let nested = checkout.join("apps/web");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::write(temp.path().join("mise.toml"), "[tools]\n").unwrap();

        assert_eq!(mise_config(&nested), None);

        fs::write(checkout.join(".mise.toml"), "[tools]\n").unwrap();
        assert_eq!(mise_config(&nested), Some(checkout.join(".mise.toml")));

        fs::write(checkout.join("mise.toml"), "[tools]\n").unwrap();
        assert_eq!(mise_config(&nested), Some(checkout.join("mise.toml")));
    }

    #[test]
    fn mise_lockfile_is_adjacent_for_both_config_spellings() {
        assert_eq!(
            mise_lockfile(Path::new("repo/mise.toml")),
            PathBuf::from("repo/mise.lock")
        );
        assert_eq!(
            mise_lockfile(Path::new("repo/.mise.toml")),
            PathBuf::from("repo/mise.lock")
        );
    }

    #[test]
    fn managed_environment_keeps_language_state_under_dot_zed() {
        let temp = tempfile::tempdir().unwrap();
        prepare_directories(temp.path(), false).unwrap();
        let environment = managed_environment(temp.path(), &options(), None).unwrap();
        let dev = temp.path().join(".zed/dev");
        let cargo_home = dev.join("cargo/home").into_os_string();
        let go_bin = dev.join("go/bin").into_os_string();
        let pub_cache = dev.join("dart/pub-cache").into_os_string();
        assert_eq!(environment.get("CARGO_HOME"), Some(&cargo_home));
        assert_eq!(environment.get("GOBIN"), Some(&go_bin));
        assert_eq!(environment.get("PUB_CACHE"), Some(&pub_cache));
        assert!(!environment.contains_key("HOME"));
    }

    #[test]
    fn isolated_home_redirects_home_and_xdg_configuration() {
        let temp = tempfile::tempdir().unwrap();
        let mut options = options();
        options.isolated_home = true;
        prepare_directories(temp.path(), true).unwrap();
        let environment = managed_environment(temp.path(), &options, None).unwrap();
        let dev = temp.path().join(".zed/dev");
        let home = dev.join("home").into_os_string();
        let config = dev.join("xdg/config").into_os_string();
        assert_eq!(environment.get("HOME"), Some(&home));
        assert_eq!(environment.get("XDG_CONFIG_HOME"), Some(&config));
    }

    #[test]
    fn an_existing_conventional_venv_is_reused() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(venv_bin_dir(&temp.path().join(".venv"))).unwrap();
        let mut options = options();
        options.python_venv = PythonVenvMode::Auto;
        let venv = ensure_python_venv(temp.path(), &options).unwrap();
        assert_eq!(venv, Some(temp.path().join(".venv")));
    }

    #[test]
    fn ai_profile_bin_is_present_in_the_managed_path() {
        let temp = tempfile::tempdir().unwrap();
        let mut options = options();
        options.profile = DevProfile::Ai;
        prepare_directories(temp.path(), false).unwrap();
        let environment = managed_environment(temp.path(), &options, None).unwrap();
        let paths: Vec<PathBuf> = env::split_paths(environment.get("PATH").unwrap()).collect();
        assert!(paths.contains(&temp.path().join(".zed/dev/profiles/ai/bin")));
    }
}
