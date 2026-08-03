//! Global executable package profiles.
//!
//! Project installs remain the default: dependencies belong beside the project
//! that declared them. Global installs are an explicit tool-distribution path.
//! Each requested top-level package gets an isolated manifestless profile under
//! `<ZED_PKG_HOME>/global/profiles/<org>/<name>`, while its hoisted executables
//! are copied atomically into one user bin directory (`~/.local/bin` by default).
//! Isolated profiles let two tools retain different transitive dependency
//! versions without turning the global environment into one giant resolver.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::manifest::is_slug;
use zed_interfaces::paths::{BIN_DIR, LOCKFILE_FILE, MODULES_DIR};

use crate::cli::{Adapter, Globals, InstallMode};
use crate::config::Config;
use crate::{dev, interactive, manifestless};

const GLOBAL_DIR: &str = "global";
const PROFILES_DIR: &str = "profiles";
const PROFILE_FILE: &str = ".zed-global-profile.json";
const STATE_FILE: &str = "managed-bins.json";
const LOCK_FILE: &str = ".lock";

#[derive(Debug, Args)]
struct GlobalArgs {
    #[command(subcommand)]
    command: GlobalAction,
}

#[derive(Debug, Subcommand)]
enum GlobalAction {
    /// Install one or more executable packages into isolated global profiles.
    Install(GlobalInstallArgs),
    /// Remove global package profiles and their managed PATH entries.
    #[command(alias = "un")]
    Uninstall(GlobalUninstallArgs),
    /// List globally installed top-level packages and exposed binaries.
    List,
    /// Print the directory where global executable copies are managed.
    #[command(name = "bin-dir")]
    BinDir,
}

#[derive(Debug, Args)]
struct GlobalInstallArgs {
    /// Package specs (`org/name[@requirement]`). With --frozen, these are
    /// optional installed-profile selectors; omit them to restore every profile.
    #[arg(value_name = "PACKAGE")]
    specs: Vec<String>,

    /// Reinstall exactly what each selected global profile lockfile pins.
    #[arg(long, env = "ZED_PKG_FROZEN")]
    frozen: bool,

    /// Materialize profile package trees by symlink or copy. Executables placed
    /// on PATH are always independently owned copies.
    #[arg(
        long,
        value_enum,
        env = "ZED_PKG_INSTALL_MODE",
        default_value = "symlink"
    )]
    install_mode: InstallMode,

    /// Run package-declared build hooks. Source-distributed CLIs generally need
    /// this once so their declared [bin] output exists.
    #[arg(long, env = "ZED_PKG_ALLOW_BUILD")]
    allow_build: bool,

    /// Select a specific polyglot target before exposing its binaries.
    #[arg(long, env = "ZED_PKG_TARGET")]
    target: Option<String>,
}

#[derive(Debug, Args)]
struct GlobalUninstallArgs {
    /// Installed top-level package identities (`org/name`). Omit to remove all.
    #[arg(value_name = "PACKAGE")]
    specs: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
struct GlobalCli {
    #[command(flatten)]
    globals: Globals,

    /// Directory placed on PATH for globally installed package executables.
    #[arg(
        long,
        global = true,
        env = "ZED_PKG_GLOBAL_BIN_DIR",
        value_name = "PATH"
    )]
    global_bin_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: GlobalCommand,
}

#[derive(Debug, Subcommand)]
enum GlobalCommand {
    /// Manage executable packages outside any one project. `zed install
    /// --global ...` and `zed uninstall --global ...` route here too.
    Global(GlobalArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Global,
    Alias {
        command_index: usize,
        global_index: usize,
        action: &'static str,
    },
    GlobalHelp {
        help_index: usize,
        target_index: usize,
    },
    RootHelp,
    Existing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileMetadata {
    package: String,
    requested: String,
}

#[derive(Debug, Clone)]
struct Profile {
    root: PathBuf,
    metadata: ProfileMetadata,
}

#[derive(Debug, Clone)]
struct DesiredBin {
    package: String,
    source: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedBin {
    package: String,
    sha256: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ManagedState {
    #[serde(default)]
    bins: BTreeMap<String, ManagedBin>,
}

struct GlobalLock(fs::File);

impl Drop for GlobalLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// Route the modular global-package command before the legacy typed command
/// parser. The canonical form is `zed global install`; the npm/cargo-style
/// `zed install --global` spelling is rewritten to the same parser and codepath.
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    match route(&args) {
        Route::Global => Some(run_cli(args)),
        Route::Alias {
            command_index,
            global_index,
            action,
        } => {
            let mut rewritten = args;
            rewritten.remove(global_index);
            let command_index = if global_index < command_index {
                command_index - 1
            } else {
                command_index
            };
            rewritten[command_index] = OsString::from("global");
            rewritten.insert(command_index + 1, OsString::from(action));
            Some(run_cli(rewritten))
        }
        Route::GlobalHelp {
            help_index,
            target_index,
        } => {
            let mut rewritten = args;
            rewritten[help_index] = OsString::from("global");
            rewritten.remove(target_index);
            rewritten.push(OsString::from("--help"));
            Some(run_cli(rewritten))
        }
        Route::RootHelp => Some(print_root_help().map(|()| 0)),
        Route::Existing => None,
    }
}

/// Add `zed global` to root help and generated completions without forcing the
/// established command enum to own a second, special installation lifecycle.
pub fn augment_root_command(command: clap::Command) -> clap::Command {
    if command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "global")
    {
        return command;
    }
    let global = <GlobalArgs as Args>::augment_args(
        clap::Command::new("global")
            .about("Install executable packages globally and manage their PATH entries"),
    )
    .arg(
        clap::Arg::new("global-bin-dir")
            .long("global-bin-dir")
            .global(true)
            .env("ZED_PKG_GLOBAL_BIN_DIR")
            .value_name("PATH")
            .help("Directory placed on PATH for global package executables"),
    );
    command.subcommand(global)
}

fn print_root_help() -> Result<()> {
    let mut command = dev::augment_root_command(augment_root_command(crate::cli::Cli::command()));
    command.print_help().context("printing zed help")?;
    println!();
    Ok(())
}

fn run_cli(args: Vec<OsString>) -> Result<i32> {
    let cli = match GlobalCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error
                .print()
                .context("printing zed global argument error")?;
            return Ok(code);
        }
    };
    let cfg = Config::from_globals(&cli.globals)?;
    let bin_dir = resolve_bin_dir(&cfg, cli.global_bin_dir.as_deref());
    match cli.command {
        GlobalCommand::Global(args) => match args.command {
            GlobalAction::Install(options) => install(&cfg, &bin_dir, options),
            GlobalAction::Uninstall(options) => uninstall(&cfg, &bin_dir, options),
            GlobalAction::List => list(&cfg, &bin_dir),
            GlobalAction::BinDir => {
                println!("{}", bin_dir.display());
                Ok(0)
            }
        },
    }
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
        "global" => Route::Global,
        "install" | "i" => global_flag(args).map_or(Route::Existing, |global_index| {
            Route::Alias {
                command_index,
                global_index,
                action: "install",
            }
        }),
        "uninstall" | "un" => global_flag(args).map_or(Route::Existing, |global_index| {
            Route::Alias {
                command_index,
                global_index,
                action: "uninstall",
            }
        }),
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target)) if target == "global" => Route::GlobalHelp {
                help_index: command_index,
                target_index,
            },
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
        "--global-bin-dir",
    ];
    OPTIONS.iter().any(|option| {
        token == *option
            || token
                .strip_prefix(option)
                .is_some_and(|remainder| remainder.starts_with('='))
    })
}

fn global_flag(args: &[OsString]) -> Option<usize> {
    args.iter()
        .position(|value| value == OsStr::new("--global"))
}

fn acquire_lock(cfg: &Config) -> Result<GlobalLock> {
    let root = global_root(cfg);
    fs::create_dir_all(&root)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(root.join(LOCK_FILE))?;
    file.lock_exclusive()
        .context("locking global package profiles")?;
    Ok(GlobalLock(file))
}

fn global_root(cfg: &Config) -> PathBuf {
    cfg.home.join(GLOBAL_DIR)
}

fn profiles_root(cfg: &Config) -> PathBuf {
    global_root(cfg).join(PROFILES_DIR)
}

fn state_path(cfg: &Config) -> PathBuf {
    global_root(cfg).join(STATE_FILE)
}

#[cfg(windows)]
fn resolve_bin_dir(cfg: &Config, explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| dirs::data_local_dir().map(|root| root.join("zed-pkg").join("bin")))
        .unwrap_or_else(|| cfg.home.join("bin"))
}

#[cfg(not(windows))]
fn resolve_bin_dir(cfg: &Config, explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("bin")))
        .unwrap_or_else(|| cfg.home.join("bin"))
}

fn parse_package_spec(spec: &str) -> Result<(String, Option<String>)> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("package spec cannot be empty");
    }
    let (key, requirement) = match spec.rsplit_once('@') {
        Some((key, requirement)) if key.contains('/') => {
            if requirement.trim().is_empty() {
                bail!("empty requirement in package spec `{spec}`");
            }
            (key, Some(requirement.trim().to_string()))
        }
        _ => (spec, None),
    };
    let (org, name) = key
        .split_once('/')
        .filter(|(org, name)| !org.is_empty() && !name.is_empty())
        .with_context(|| format!("invalid package spec `{spec}` (expected org/name[@requirement])"))?;
    if name.contains('/') || !is_slug(org) || !is_slug(name) {
        bail!("invalid package identity `{key}`; expected lowercase slug org/name");
    }
    Ok((format!("{org}/{name}"), requirement))
}

fn profile_root(cfg: &Config, key: &str) -> Result<PathBuf> {
    let (org, name) = key
        .split_once('/')
        .context("validated package identity contains a slash")?;
    if !is_slug(org) || !is_slug(name) {
        bail!("invalid global package identity `{key}`");
    }
    Ok(profiles_root(cfg).join(org).join(name))
}

fn install(cfg: &Config, bin_dir: &Path, options: GlobalInstallArgs) -> Result<i32> {
    let _lock = acquire_lock(cfg)?;
    if options.frozen {
        let profiles = selected_profiles(cfg, &options.specs)?;
        if profiles.is_empty() {
            bail!("no global package profiles are installed");
        }
        for profile in &profiles {
            manifestless::install(
                &profile.root,
                cfg,
                &[],
                true,
                options.install_mode,
                Adapter::None,
                options.allow_build,
                options.target.as_deref(),
                true,
                true,
            )?;
        }
    } else {
        if options.specs.is_empty() {
            bail!(
                "global install needs one or more `org/name[@requirement]` package specs; use --frozen to restore existing profiles"
            );
        }
        for requested in &options.specs {
            let (key, _) = parse_package_spec(requested)?;
            let root = profile_root(cfg, &key)?;
            let existed = root.exists();
            fs::create_dir_all(&root)?;
            let result = manifestless::install(
                &root,
                cfg,
                std::slice::from_ref(requested),
                false,
                options.install_mode,
                Adapter::None,
                options.allow_build,
                options.target.as_deref(),
                true,
                true,
            );
            if let Err(error) = result {
                if !existed {
                    let _ = fs::remove_dir_all(&root);
                }
                return Err(error);
            }
            write_metadata(
                &root,
                &ProfileMetadata {
                    package: key.clone(),
                    requested: requested.clone(),
                },
            )?;
            let bin_count = profile_bins(&root)?.len();
            if bin_count == 0 {
                eprintln!(
                    "warning: {key} currently exposes no built [bin] entries in this profile; if it declares a [build] step, reinstall with --allow-build"
                );
            }
        }
    }

    let profiles = discover_profiles(cfg)?;
    let installed = sync_bins(cfg, bin_dir, &profiles)?;
    print_path_guidance(bin_dir);
    println!(
        "{} global package profile(s); {} executable(s) managed in {}",
        profiles.len(),
        installed,
        bin_dir.display()
    );
    Ok(0)
}

fn uninstall(cfg: &Config, bin_dir: &Path, options: GlobalUninstallArgs) -> Result<i32> {
    let _lock = acquire_lock(cfg)?;
    let profiles = selected_profiles(cfg, &options.specs)?;
    if profiles.is_empty() {
        println!("no matching global package profiles");
        return Ok(0);
    }
    for profile in &profiles {
        interactive::confirm(
            cfg.interactive,
            &format!("remove global package profile {}", profile.metadata.package),
        )?;
        fs::remove_dir_all(&profile.root).with_context(|| {
            format!(
                "removing global profile {} at {}",
                profile.metadata.package,
                profile.root.display()
            )
        })?;
        println!("uninstalled {}", profile.metadata.package);
    }
    let remaining = discover_profiles(cfg)?;
    let installed = sync_bins(cfg, bin_dir, &remaining)?;
    println!(
        "{} global package profile(s) remain; {} executable(s) managed in {}",
        remaining.len(),
        installed,
        bin_dir.display()
    );
    Ok(0)
}

fn list(cfg: &Config, bin_dir: &Path) -> Result<i32> {
    let profiles = discover_profiles(cfg)?;
    if profiles.is_empty() {
        println!("no global package profiles installed");
    }
    for profile in profiles {
        let version = locked_root_version(&profile).unwrap_or_else(|| "unknown".to_string());
        let mut bins: Vec<String> = profile_bins(&profile.root)?.into_keys().collect();
        bins.sort();
        println!(
            "{}@{} (requested `{}`; bins: {})",
            profile.metadata.package,
            version,
            profile.metadata.requested,
            if bins.is_empty() {
                "none".to_string()
            } else {
                bins.join(", ")
            }
        );
    }
    println!("global bin directory: {}", bin_dir.display());
    Ok(0)
}

fn selected_profiles(cfg: &Config, specs: &[String]) -> Result<Vec<Profile>> {
    let profiles = discover_profiles(cfg)?;
    if specs.is_empty() {
        return Ok(profiles);
    }
    let mut by_key: BTreeMap<String, Profile> = profiles
        .into_iter()
        .map(|profile| (profile.metadata.package.clone(), profile))
        .collect();
    let mut selected = Vec::new();
    for spec in specs {
        let (key, requirement) = parse_package_spec(spec)?;
        if requirement.is_some() {
            bail!(
                "global profile selectors must be package identities without versions (got `{spec}`)"
            );
        }
        let profile = by_key
            .remove(&key)
            .with_context(|| format!("global package `{key}` is not installed"))?;
        selected.push(profile);
    }
    Ok(selected)
}

fn discover_profiles(cfg: &Config) -> Result<Vec<Profile>> {
    let root = profiles_root(cfg);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut profiles = Vec::new();
    for org in fs::read_dir(&root)? {
        let org = org?;
        if !org.file_type()?.is_dir() {
            continue;
        }
        let org_name = org.file_name().to_string_lossy().to_string();
        if !is_slug(&org_name) {
            continue;
        }
        for package in fs::read_dir(org.path())? {
            let package = package?;
            if !package.file_type()?.is_dir() {
                continue;
            }
            let package_name = package.file_name().to_string_lossy().to_string();
            if !is_slug(&package_name) {
                continue;
            }
            let metadata_path = package.path().join(PROFILE_FILE);
            if !metadata_path.is_file() {
                continue;
            }
            let metadata: ProfileMetadata = serde_json::from_slice(&fs::read(&metadata_path)?)
                .with_context(|| format!("parsing {}", metadata_path.display()))?;
            let expected = format!("{org_name}/{package_name}");
            if metadata.package != expected {
                bail!(
                    "{} claims package `{}` but its managed path requires `{expected}`",
                    metadata_path.display(),
                    metadata.package
                );
            }
            profiles.push(Profile {
                root: package.path(),
                metadata,
            });
        }
    }
    profiles.sort_by(|left, right| left.metadata.package.cmp(&right.metadata.package));
    Ok(profiles)
}

fn write_metadata(root: &Path, metadata: &ProfileMetadata) -> Result<()> {
    fs::create_dir_all(root)?;
    atomic_write(
        &root.join(PROFILE_FILE),
        &serde_json::to_vec_pretty(metadata)?,
    )
}

fn locked_root_version(profile: &Profile) -> Option<String> {
    let text = fs::read_to_string(profile.root.join(LOCKFILE_FILE)).ok()?;
    let lock = Lockfile::parse(&text).ok()?;
    let (org, name) = profile.metadata.package.split_once('/')?;
    lock.find(org, name).map(|package| package.version.clone())
}

fn profile_bins(root: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let bin_dir = root.join(MODULES_DIR).join(BIN_DIR);
    if !bin_dir.is_dir() {
        return Ok(BTreeMap::new());
    }
    let mut bins = BTreeMap::new();
    for entry in fs::read_dir(&bin_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("global bin name is not valid UTF-8"))?;
        validate_bin_name(&name)?;
        bins.insert(name, entry.path());
    }
    Ok(bins)
}

fn validate_bin_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '+'))
    {
        bail!("unsafe global bin name `{name}`");
    }
    Ok(())
}

fn collect_desired_bins(profiles: &[Profile]) -> Result<BTreeMap<String, DesiredBin>> {
    let mut desired: BTreeMap<String, DesiredBin> = BTreeMap::new();
    for profile in profiles {
        for (logical_name, source) in profile_bins(&profile.root)? {
            let destination = destination_name(&logical_name);
            if let Some(existing) = desired.get(&destination) {
                bail!(
                    "global bin collision for `{destination}` between {} and {}; uninstall one package or use separate ZED_PKG_GLOBAL_BIN_DIR values",
                    existing.package,
                    profile.metadata.package
                );
            }
            desired.insert(
                destination,
                DesiredBin {
                    package: profile.metadata.package.clone(),
                    sha256: hash_file(&source)?,
                    source,
                },
            );
        }
    }
    Ok(desired)
}

#[cfg(windows)]
fn destination_name(logical_name: &str) -> String {
    if logical_name.to_ascii_lowercase().ends_with(".exe") {
        logical_name.to_string()
    } else {
        format!("{logical_name}.exe")
    }
}

#[cfg(not(windows))]
fn destination_name(logical_name: &str) -> String {
    logical_name.to_string()
}

fn load_state(cfg: &Config) -> Result<ManagedState> {
    let path = state_path(cfg);
    if !path.is_file() {
        return Ok(ManagedState::default());
    }
    serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("parsing managed global-bin state at {}", path.display()))
}

fn sync_bins(cfg: &Config, bin_dir: &Path, profiles: &[Profile]) -> Result<usize> {
    let desired = collect_desired_bins(profiles)?;
    let previous = load_state(cfg)?;
    fs::create_dir_all(bin_dir)?;

    for (name, managed) in &previous.bins {
        if desired.contains_key(name) {
            continue;
        }
        let destination = bin_dir.join(name);
        if !destination.exists() {
            continue;
        }
        let current = hash_file(&destination)?;
        if current == managed.sha256 {
            fs::remove_file(&destination)?;
        } else {
            eprintln!(
                "warning: leaving {} because it changed after Zed installed it",
                destination.display()
            );
        }
    }

    let mut next = ManagedState::default();
    for (name, wanted) in desired {
        let destination = bin_dir.join(&name);
        if destination.exists() {
            let current = hash_file(&destination)?;
            let owned = previous
                .bins
                .get(&name)
                .is_some_and(|managed| managed.sha256 == current);
            if !owned {
                bail!(
                    "refusing to replace unmanaged global executable {}; choose another --global-bin-dir or remove the collision explicitly",
                    destination.display()
                );
            }
        }
        atomic_copy(&wanted.source, &destination)?;
        next.bins.insert(
            name,
            ManagedBin {
                package: wanted.package,
                sha256: wanted.sha256,
            },
        );
    }

    atomic_write(&state_path(cfg), &serde_json::to_vec_pretty(&next)?)?;
    Ok(next.bins.len())
}

fn atomic_copy(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("global executable destination has no parent")?;
    fs::create_dir_all(parent)?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("global executable name is not valid UTF-8")?;
    let temporary = parent.join(format!(
        ".{file_name}.zed-tmp-{}",
        uuid::Uuid::new_v4()
    ));
    fs::copy(source, &temporary).with_context(|| {
        format!(
            "copying global executable {} to {}",
            source.display(),
            temporary.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&temporary)?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(&temporary, permissions)?;
    }
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| {
            format!(
                "promoting global executable into {}",
                destination.display()
            )
        });
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("managed file has no parent")?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("managed file name is not valid UTF-8")?;
    let temporary = parent.join(format!(
        ".{file_name}.zed-tmp-{}",
        uuid::Uuid::new_v4()
    ));
    fs::write(&temporary, bytes)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("promoting {}", path.display()));
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[cfg(windows)]
fn print_path_guidance(bin_dir: &Path) {
    if path_contains(bin_dir) {
        return;
    }
    eprintln!(
        "warning: {} is not currently on PATH; add it to the user PATH once",
        bin_dir.display()
    );
}

#[cfg(not(windows))]
fn print_path_guidance(bin_dir: &Path) {
    if path_contains(bin_dir) {
        return;
    }
    eprintln!(
        "warning: {} is not currently on PATH; add it once, for example:\n  export PATH=\"{}:$PATH\"",
        bin_dir.display(),
        bin_dir.display()
    );
}

fn path_contains(bin_dir: &Path) -> bool {
    let expected = fs::canonicalize(bin_dir).unwrap_or_else(|_| bin_dir.to_path_buf());
    env::var_os("PATH").is_some_and(|value| {
        env::split_paths(&value)
            .any(|entry| fs::canonicalize(&entry).unwrap_or(entry) == expected)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(home: &Path) -> Config {
        Config {
            registry: "file:///unused".to_string(),
            home: home.to_path_buf(),
            token: None,
            auth_url: "https://localhost/shared-auth".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        }
    }

    fn add_profile(home: &Path, key: &str, bin_name: &str, contents: &[u8]) -> Profile {
        let cfg = config(home);
        let root = profile_root(&cfg, key).unwrap();
        let bin = root.join(MODULES_DIR).join(BIN_DIR).join(bin_name);
        fs::create_dir_all(bin.parent().unwrap()).unwrap();
        fs::write(&bin, contents).unwrap();
        let metadata = ProfileMetadata {
            package: key.to_string(),
            requested: key.to_string(),
        };
        write_metadata(&root, &metadata).unwrap();
        Profile { root, metadata }
    }

    #[test]
    fn package_specs_are_strict_and_traversal_safe() {
        assert_eq!(
            parse_package_spec("acme/tool@^1").unwrap(),
            ("acme/tool".to_string(), Some("^1".to_string()))
        );
        assert_eq!(
            parse_package_spec("acme/tool").unwrap(),
            ("acme/tool".to_string(), None)
        );
        for invalid in [
            "tool",
            "acme/tool/extra",
            "../tool",
            "acme/../tool",
            "Acme/tool",
        ] {
            assert!(parse_package_spec(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn alias_route_rewrites_install_global() {
        let args = vec![
            OsString::from("zed"),
            OsString::from("install"),
            OsString::from("--global"),
            OsString::from("acme/tool"),
        ];
        assert!(matches!(
            route(&args),
            Route::Alias {
                action: "install",
                ..
            }
        ));
    }

    #[test]
    fn global_bins_are_copied_and_tampered_files_are_preserved() {
        let home = tempfile::tempdir().unwrap();
        let bin_dir = home.path().join("path-bin");
        let cfg = config(home.path());
        let profile = add_profile(home.path(), "acme/tool", "acme-tool", b"first");

        assert_eq!(
            sync_bins(&cfg, &bin_dir, std::slice::from_ref(&profile)).unwrap(),
            1
        );
        let installed = bin_dir.join(destination_name("acme-tool"));
        assert_eq!(fs::read(&installed).unwrap(), b"first");

        fs::write(&installed, b"user replacement").unwrap();
        assert_eq!(sync_bins(&cfg, &bin_dir, &[]).unwrap(), 0);
        assert_eq!(fs::read(&installed).unwrap(), b"user replacement");
    }

    #[test]
    fn bin_collisions_fail_before_path_mutation() {
        let home = tempfile::tempdir().unwrap();
        let first = add_profile(home.path(), "acme/one", "tool", b"one");
        let second = add_profile(home.path(), "acme/two", "tool", b"two");
        let error = collect_desired_bins(&[first, second]).unwrap_err();
        assert!(error.to_string().contains("collision"));
    }

    #[test]
    fn explicit_global_bin_directory_wins() {
        let home = tempfile::tempdir().unwrap();
        let cfg = config(home.path());
        let explicit = home.path().join("custom-bin");
        assert_eq!(resolve_bin_dir(&cfg, Some(&explicit)), explicit);
    }
}
