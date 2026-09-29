//! Explicit local package links for iterative development.
//!
//! This is Zed's deterministic counterpart to npm's two-stage `npm link`
//! workflow:
//!
//! 1. `zed link .` (or `zed link`) registers a canonical package working tree
//!    beneath `ZED_PKG_HOME` by its authored `org/name` identity.
//! 2. `zed link org/name` in a consumer installs a live directory symlink into
//!    `zed_modules/<org>/<name>` and, for Node projects, into
//!    `node_modules/@<org>/<name>`.
//!
//! Link state is deliberately outside `.zpkg.toml` and `.zpkg.lock`. Ordinary
//! and frozen installs never discover this registry implicitly; local mutable
//! state participates only after an explicit `zed link` command.

use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use flags2env::BundledFlags2Env;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zed_interfaces::paths::{MANIFEST_FILE, MODULES_DIR};
use zed_lock::{LockClass, LockGuard, LockManager, LockRequest};

use crate::cli::Globals;
use crate::config::Config;

const LINK_CONTRACT: &str = include_str!("../.link-cli-flags.toml");
const LINKS_DIR: &str = "links";
const LINKS_LOCK_FILE: &str = ".lock";
const CONSUMER_STATE_DIR: &str = ".zed/local-links";
const CONSUMER_BACKUP_DIR: &str = ".zed/local-link-backups";
const RECEIPT_SCHEMA_VERSION: u32 = 1;
const MAX_REGISTERED_LINKS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum LocalLinkAdapter {
    /// Detect a Node consumer from package.json; otherwise use zed_modules only.
    Auto,
    /// Install only into zed_modules/<org>/<name>.
    None,
    /// Also expose the package as node_modules/@<org>/<name>.
    Node,
}

#[derive(Debug, Args)]
struct LinkArgs {
    /// Registered package name (`org/name` or unique bare name), or a local
    /// package path. `.` registers the current package without consuming it.
    #[arg(value_name = "PACKAGE_OR_PATH")]
    target: Option<String>,

    /// Consumer projection. Auto adds the Node projection when package.json exists.
    #[arg(
        long,
        value_enum,
        env = "ZED_PKG_LINK_ADAPTER",
        default_value = "auto"
    )]
    adapter: LocalLinkAdapter,
}

#[derive(Debug, Args)]
struct UnlinkArgs {
    /// Consumer package name. Omit it to unregister the current package.
    #[arg(value_name = "PACKAGE")]
    target: Option<String>,

    /// Remove a machine-wide registration instead of a project-local link.
    #[arg(
        long,
        env = "ZED_PKG_LINK_GLOBAL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = clap::builder::BoolishValueParser::new(),
        action = clap::ArgAction::Set
    )]
    global: bool,
}

#[derive(Debug, Args)]
struct LinksArgs {
    /// Emit deterministic JSON.
    #[arg(
        long,
        env = "ZED_PKG_LINK_JSON",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        default_value = "false",
        value_parser = clap::builder::BoolishValueParser::new(),
        action = clap::ArgAction::Set
    )]
    json: bool,
}

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
struct LinkCli {
    #[command(flatten)]
    globals: Globals,

    #[command(subcommand)]
    command: LinkCommand,
}

#[derive(Debug, Subcommand)]
enum LinkCommand {
    /// Register a package working tree or consume a registered live package link.
    #[command(visible_alias = "ln")]
    Link(LinkArgs),
    /// Remove a consumer link or unregister a package working tree.
    Unlink(UnlinkArgs),
    /// List machine-wide local package registrations.
    Links(LinksArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Link,
    Help {
        help_index: usize,
        target_index: usize,
    },
    Existing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Registration {
    schema_version: u32,
    package: String,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PreviousState {
    Absent,
    Symlink { target: String },
    Directory { backup: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ProjectionReceipt {
    destination: String,
    previous: PreviousState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ConsumerReceipt {
    schema_version: u32,
    package: String,
    source: String,
    projections: Vec<ProjectionReceipt>,
}

#[derive(Debug, Clone, Serialize)]
struct RegistrationStatus {
    package: String,
    source: String,
    status: String,
}

/// Route local-link commands before the established typed project parser.
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    match route(&args) {
        Route::Link => Some(run_cli(args)),
        Route::Help {
            help_index,
            target_index,
        } => {
            let mut rewritten = args;
            let target = rewritten.get(target_index)?.clone();
            rewritten[help_index] = target;
            rewritten.remove(target_index);
            rewritten.push(OsString::from("--help"));
            Some(run_cli(rewritten))
        }
        Route::Existing => None,
    }
}

/// Add local-link commands to root help and generated shell completions.
pub fn augment_root_command(mut command: clap::Command) -> clap::Command {
    if !command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "link")
    {
        let link = <LinkArgs as Args>::augment_args(
            clap::Command::new("link")
                .visible_alias("ln")
                .about("Register or consume a live local package working tree"),
        );
        command = command.subcommand(link);
    }
    if !command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "unlink")
    {
        let unlink = <UnlinkArgs as Args>::augment_args(
            clap::Command::new("unlink")
                .about("Remove a consumer link or a machine-wide registration"),
        );
        command = command.subcommand(unlink);
    }
    if !command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "links")
    {
        let links = <LinksArgs as Args>::augment_args(
            clap::Command::new("links").about("List machine-wide local package registrations"),
        );
        command = command.subcommand(links);
    }
    command
}

fn run_cli(args: Vec<OsString>) -> Result<i32> {
    let string_args = utf8_args(&args)?;
    validate_link_flags(&string_args)?;

    let cli = match LinkCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error
                .print()
                .context("printing zed local-link argument error")?;
            return Ok(code);
        }
    };

    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = env::current_dir().context("reading current directory")?;
    match cli.command {
        LinkCommand::Link(options) => run_link(&cwd, &cfg.home, options),
        LinkCommand::Unlink(options) => run_unlink(&cwd, &cfg.home, options),
        LinkCommand::Links(options) => run_links(&cfg.home, options),
    }
}

fn run_link(cwd: &Path, home: &Path, options: LinkArgs) -> Result<i32> {
    match options.target.as_deref() {
        None | Some(".") => {
            let registration = register(home, cwd)?;
            println!("linked {} -> {}", registration.package, registration.source);
            Ok(0)
        }
        Some(target) if is_explicit_path(target) => {
            let source = resolve_cli_path(cwd, target);
            let registration = register(home, &source)?;
            let source_canonical = canonical_source(&source)?;
            let cwd_canonical = canonical_source(cwd)?;
            if source_canonical == cwd_canonical {
                println!("linked {} -> {}", registration.package, registration.source);
                return Ok(0);
            }
            let receipt = consume(
                &cwd_canonical,
                home,
                &registration.package,
                options.adapter,
            )?;
            println!(
                "linked {} -> {} ({} projection{})",
                receipt.package,
                receipt.source,
                receipt.projections.len(),
                if receipt.projections.len() == 1 { "" } else { "s" }
            );
            Ok(0)
        }
        Some(target) => {
            let receipt = consume(cwd, home, target, options.adapter)?;
            println!(
                "linked {} -> {} ({} projection{})",
                receipt.package,
                receipt.source,
                receipt.projections.len(),
                if receipt.projections.len() == 1 { "" } else { "s" }
            );
            Ok(0)
        }
    }
}

fn run_unlink(cwd: &Path, home: &Path, options: UnlinkArgs) -> Result<i32> {
    match (options.target.as_deref(), options.global) {
        (None, _) | (Some("."), _) => {
            let package = package_identity(cwd)?;
            unregister(home, &package)?;
            println!("unregistered {package}");
            Ok(0)
        }
        (Some(target), true) => {
            let package = resolve_registered_key(home, target)?;
            unregister(home, &package)?;
            println!("unregistered {package}");
            Ok(0)
        }
        (Some(target), false) => {
            let package = normalize_or_resolve_key(home, target)?;
            unlink_consumer(cwd, &package)?;
            println!("unlinked {package}");
            Ok(0)
        }
    }
}

fn run_links(home: &Path, options: LinksArgs) -> Result<i32> {
    let statuses = list_registrations(home)?;
    if options.json {
        serde_json::to_writer_pretty(io::stdout().lock(), &statuses)
            .context("writing local-link JSON")?;
        println!();
        return Ok(0);
    }
    for status in statuses {
        println!("{}\t{}\t{}", status.package, status.status, status.source);
    }
    Ok(0)
}

fn register(home: &Path, source: &Path) -> Result<Registration> {
    let canonical = canonical_source(source)?;
    let package = package_identity(&canonical)?;
    let source = path_to_utf8(&canonical, "local package source")?;
    let registration = Registration {
        schema_version: RECEIPT_SCHEMA_VERSION,
        package: package.clone(),
        source,
    };

    let _guard = acquire_registry_lock(home)?;
    let path = registration_path(home, &package)?;
    write_json_atomic(&path, &registration)?;
    Ok(registration)
}

fn unregister(home: &Path, package: &str) -> Result<()> {
    let package = normalize_key(package)?;
    let _guard = acquire_registry_lock(home)?;
    let path = registration_path(home, &package)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("local package `{package}` is not registered")
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    ensure!(
        metadata.file_type().is_file(),
        "refusing to remove non-regular local-link registration {}",
        path.display()
    );
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

fn consume(
    project: &Path,
    home: &Path,
    requested: &str,
    adapter: LocalLinkAdapter,
) -> Result<ConsumerReceipt> {
    let registration = read_registered(home, requested)?;
    let source = validate_registration_source(project, &registration)?;
    let project = canonical_source(project).context("consumer project must be a directory")?;
    let adapter = effective_adapter(&project, adapter);
    crate::project_lock::with_lock(&project, "link local package working tree", || {
        consume_locked(&project, &registration, &source, adapter)
    })
}

fn consume_locked(
    project: &Path,
    registration: &Registration,
    source: &Path,
    adapter: LocalLinkAdapter,
) -> Result<ConsumerReceipt> {
    let package = &registration.package;
    if consumer_receipt_path(project, package)?.exists() {
        unlink_consumer_locked(project, package)?;
    }

    let projections = projection_destinations(project, package, adapter)?;
    let mut applied = Vec::new();
    for (label, destination) in projections {
        match apply_projection(project, source, package, label, &destination) {
            Ok(receipt) => applied.push(receipt),
            Err(error) => {
                rollback_projections(project, source, &applied)?;
                return Err(error);
            }
        }
    }

    let receipt = ConsumerReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        package: package.clone(),
        source: registration.source.clone(),
        projections: applied,
    };
    write_json_atomic(&consumer_receipt_path(project, package)?, &receipt)?;
    Ok(receipt)
}

fn unlink_consumer(project: &Path, package: &str) -> Result<()> {
    let package = normalize_key(package)?;
    let project = canonical_source(project).context("consumer project must be a directory")?;
    crate::project_lock::with_lock(&project, "unlink local package working tree", || {
        unlink_consumer_locked(&project, &package)
    })
}

fn unlink_consumer_locked(project: &Path, package: &str) -> Result<()> {
    let path = consumer_receipt_path(project, package)?;
    let receipt: ConsumerReceipt = read_json_regular(&path)
        .with_context(|| format!("no managed consumer link for `{package}`"))?;
    validate_consumer_receipt(&receipt, package)?;
    let source = canonical_source(Path::new(&receipt.source)).with_context(|| {
        format!(
            "local link source for `{package}` is missing; refusing to guess what consumer paths own"
        )
    })?;

    for projection in receipt.projections.iter().rev() {
        restore_projection(project, &source, projection)?;
    }
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

fn rollback_projections(
    project: &Path,
    source: &Path,
    projections: &[ProjectionReceipt],
) -> Result<()> {
    let mut first_error = None;
    for projection in projections.iter().rev() {
        if let Err(error) = restore_projection(project, source, projection) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error).context("rolling back partially applied local package link");
    }
    Ok(())
}

fn apply_projection(
    project: &Path,
    source: &Path,
    package: &str,
    label: &str,
    lexical_destination: &Path,
) -> Result<ProjectionReceipt> {
    let canonical_source = canonical_source(source)?;
    let destination = safe_destination(project, &canonical_source, lexical_destination)?;

    let destination_relative = relative_utf8(project, &destination, "local-link destination")?;
    let backup_relative = backup_relative_path(package, label)?;
    let previous = detach_previous(project, &destination, &backup_relative)?;
    if let Err(error) = create_directory_symlink(&canonical_source, &destination) {
        restore_previous(project, &destination, &previous)?;
        return Err(error);
    }
    Ok(ProjectionReceipt {
        destination: destination_relative,
        previous,
    })
}

fn restore_projection(
    project: &Path,
    source: &Path,
    projection: &ProjectionReceipt,
) -> Result<()> {
    let destination = checked_project_relative(project, &projection.destination)?;
    remove_expected_live_link(&destination, source)?;
    restore_previous(project, &destination, &projection.previous)
}

fn detach_previous(
    project: &Path,
    destination: &Path,
    backup_relative: &str,
) -> Result<PreviousState> {
    let metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreviousState::Absent);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading existing {}", destination.display()));
        }
    };

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(destination)
            .with_context(|| format!("reading symlink {}", destination.display()))?;
        let target = path_to_utf8(&target, "previous package symlink target")?;
        fs::remove_file(destination)
            .with_context(|| format!("detaching previous symlink {}", destination.display()))?;
        return Ok(PreviousState::Symlink { target });
    }

    if metadata.is_dir() {
        let backup = checked_project_relative(project, backup_relative)?;
        if fs::symlink_metadata(&backup).is_ok() {
            bail!(
                "local-link backup already exists at {}; run `zed unlink` or inspect stale state before retrying",
                backup.display()
            );
        }
        if let Some(parent) = backup.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating local-link backup {}", parent.display()))?;
        }
        fs::rename(destination, &backup).with_context(|| {
            format!(
                "moving existing package directory {} to reversible local-link backup {}",
                destination.display(),
                backup.display()
            )
        })?;
        return Ok(PreviousState::Directory {
            backup: backup_relative.to_string(),
        });
    }

    bail!(
        "refusing to replace non-directory package path {} with a local link",
        destination.display()
    )
}

fn restore_previous(
    project: &Path,
    destination: &Path,
    previous: &PreviousState,
) -> Result<()> {
    match previous {
        PreviousState::Absent => Ok(()),
        PreviousState::Symlink { target } => {
            ensure_destination_absent(destination)?;
            let target = PathBuf::from(target);
            create_directory_symlink(&target, destination)
                .with_context(|| format!("restoring previous symlink {}", destination.display()))
        }
        PreviousState::Directory { backup } => {
            ensure_destination_absent(destination)?;
            let backup = checked_project_relative(project, backup)?;
            let metadata = fs::symlink_metadata(&backup).with_context(|| {
                format!(
                    "local-link backup {} is missing; refusing destructive unlink",
                    backup.display()
                )
            })?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "local-link backup {} is not a regular directory",
                backup.display()
            );
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&backup, destination).with_context(|| {
                format!(
                    "restoring package directory {} from {}",
                    destination.display(),
                    backup.display()
                )
            })
        }
    }
}

fn remove_expected_live_link(destination: &Path, source: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(destination).with_context(|| {
        format!(
            "managed local-link destination {} is missing; refusing to restore over unknown state",
            destination.display()
        )
    })?;
    ensure!(
        metadata.file_type().is_symlink(),
        "managed local-link destination {} is no longer a symlink; refusing to remove it",
        destination.display()
    );
    let raw = fs::read_link(destination)
        .with_context(|| format!("reading managed symlink {}", destination.display()))?;
    let resolved = if raw.is_absolute() {
        raw
    } else {
        destination
            .parent()
            .context("managed symlink has no parent")?
            .join(raw)
    };
    let resolved = resolved.canonicalize().with_context(|| {
        format!(
            "managed symlink {} is dangling; refusing to remove it without source verification",
            destination.display()
        )
    })?;
    ensure!(
        resolved == source,
        "managed local-link destination {} now points to {} instead of {}; refusing to remove a path changed by another tool",
        destination.display(),
        resolved.display(),
        source.display()
    );
    fs::remove_file(destination)
        .with_context(|| format!("removing managed symlink {}", destination.display()))
}

fn ensure_destination_absent(destination: &Path) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(_) => bail!(
            "cannot restore previous package state because {} is occupied",
            destination.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("checking {}", destination.display())),
    }
}

fn projection_destinations(
    project: &Path,
    package: &str,
    adapter: LocalLinkAdapter,
) -> Result<Vec<(&'static str, PathBuf)>> {
    let (org, name) = key_parts(package)?;
    let mut destinations = vec![(
        "zed_modules",
        project.join(MODULES_DIR).join(org).join(name),
    )];
    if adapter == LocalLinkAdapter::Node {
        destinations.push((
            "node_modules",
            project
                .join("node_modules")
                .join(format!("@{org}"))
                .join(name),
        ));
    }
    Ok(destinations)
}

fn effective_adapter(project: &Path, adapter: LocalLinkAdapter) -> LocalLinkAdapter {
    match adapter {
        LocalLinkAdapter::Auto if project.join("package.json").is_file() => LocalLinkAdapter::Node,
        LocalLinkAdapter::Auto => LocalLinkAdapter::None,
        LocalLinkAdapter::None => LocalLinkAdapter::None,
        LocalLinkAdapter::Node => LocalLinkAdapter::Node,
    }
}

fn read_registered(home: &Path, requested: &str) -> Result<Registration> {
    let _guard = acquire_registry_lock(home)?;
    let package = normalize_or_resolve_key_locked(home, requested)?;
    let path = registration_path(home, &package)?;
    let registration: Registration = read_json_regular(&path)
        .with_context(|| format!("local package `{package}` is not registered"))?;
    validate_registration_shape(&registration, &package)?;
    Ok(registration)
}

fn resolve_registered_key(home: &Path, requested: &str) -> Result<String> {
    let _guard = acquire_registry_lock(home)?;
    normalize_or_resolve_key_locked(home, requested)
}

fn normalize_or_resolve_key(home: &Path, requested: &str) -> Result<String> {
    let _guard = acquire_registry_lock(home)?;
    normalize_or_resolve_key_locked(home, requested)
}

fn normalize_or_resolve_key_locked(home: &Path, requested: &str) -> Result<String> {
    let trimmed = requested.trim();
    if trimmed.contains('/') {
        return normalize_key(trimmed);
    }
    ensure!(!trimmed.is_empty(), "local package name cannot be empty");

    let matches = registration_keys_locked(home)?
        .into_iter()
        .filter(|key| key_parts(key).is_ok_and(|(_, name)| name == trimmed))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!(
            "no registered local package has bare name `{trimmed}`; use `zed links` or an `org/name` identity"
        ),
        _ => bail!(
            "bare local package name `{trimmed}` is ambiguous: {}; use `org/name`",
            matches.join(", ")
        ),
    }
}

fn list_registrations(home: &Path) -> Result<Vec<RegistrationStatus>> {
    let _guard = acquire_registry_lock(home)?;
    let mut statuses = Vec::new();
    for package in registration_keys_locked(home)? {
        let path = registration_path(home, &package)?;
        let registration: Registration = read_json_regular(&path)?;
        validate_registration_shape(&registration, &package)?;
        let status = match validate_registration_source_without_project(&registration) {
            Ok(_) => "ok".to_string(),
            Err(error) => format!("invalid: {error:#}"),
        };
        statuses.push(RegistrationStatus {
            package,
            source: registration.source,
            status,
        });
    }
    Ok(statuses)
}

fn registration_keys_locked(home: &Path) -> Result<Vec<String>> {
    let root = links_root(home);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    let mut orgs = fs::read_dir(&root)
        .with_context(|| format!("reading local-link registry {}", root.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    orgs.sort_by_key(fs::DirEntry::file_name);

    for org_entry in orgs {
        if keys.len() >= MAX_REGISTERED_LINKS {
            bail!("local-link registry exceeds {MAX_REGISTERED_LINKS} registrations");
        }
        let metadata = fs::symlink_metadata(org_entry.path())?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let org = org_entry.file_name().to_string_lossy().into_owned();
        let mut packages = fs::read_dir(org_entry.path())?
            .collect::<std::io::Result<Vec<_>>>()?;
        packages.sort_by_key(fs::DirEntry::file_name);
        for package_entry in packages {
            if keys.len() >= MAX_REGISTERED_LINKS {
                bail!("local-link registry exceeds {MAX_REGISTERED_LINKS} registrations");
            }
            let metadata = fs::symlink_metadata(package_entry.path())?;
            if !metadata.file_type().is_file() {
                continue;
            }
            let filename = package_entry.file_name().to_string_lossy().into_owned();
            let Some(name) = filename.strip_suffix(".json") else {
                continue;
            };
            let key = format!("{org}/{name}");
            normalize_key(&key)?;
            keys.push(key);
        }
    }
    keys.sort();
    Ok(keys)
}

fn validate_registration_source(project: &Path, registration: &Registration) -> Result<PathBuf> {
    let mut raw = BTreeMap::new();
    raw.insert(registration.package.clone(), registration.source.clone());
    let resolved = crate::local_overrides::resolve(project, MODULES_DIR, &raw)?;
    let source = resolved
        .get(&registration.package)
        .context("local-link source resolution omitted registered package")?
        .clone();
    validate_registration_identity(registration, &source)?;
    Ok(source)
}

fn validate_registration_source_without_project(registration: &Registration) -> Result<PathBuf> {
    let source = canonical_source(Path::new(&registration.source))?;
    validate_registration_identity(registration, &source)?;
    Ok(source)
}

fn validate_registration_identity(registration: &Registration, source: &Path) -> Result<()> {
    let actual = package_identity(source)?;
    ensure!(
        actual == registration.package,
        "registered source {} now declares `{actual}` instead of `{}`",
        source.display(),
        registration.package
    );
    Ok(())
}

fn validate_registration_shape(registration: &Registration, expected: &str) -> Result<()> {
    ensure!(
        registration.schema_version == RECEIPT_SCHEMA_VERSION,
        "unsupported local-link registration schema {}",
        registration.schema_version
    );
    let package = normalize_key(&registration.package)?;
    ensure!(
        package == expected,
        "local-link registration identity `{package}` does not match registry path `{expected}`"
    );
    ensure!(
        !registration.source.trim().is_empty(),
        "local-link registration `{expected}` has an empty source"
    );
    Ok(())
}

fn validate_consumer_receipt(receipt: &ConsumerReceipt, expected: &str) -> Result<()> {
    ensure!(
        receipt.schema_version == RECEIPT_SCHEMA_VERSION,
        "unsupported consumer local-link receipt schema {}",
        receipt.schema_version
    );
    ensure!(
        receipt.package == expected,
        "consumer local-link receipt for `{}` does not match requested `{expected}`",
        receipt.package
    );
    ensure!(
        !receipt.projections.is_empty(),
        "consumer local-link receipt for `{expected}` has no managed projections"
    );
    Ok(())
}

fn package_identity(source: &Path) -> Result<String> {
    let manifest = source.join(MANIFEST_FILE);
    let metadata = fs::symlink_metadata(&manifest)
        .with_context(|| format!("local package has no {}", manifest.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "local package manifest {} must be a regular file",
        manifest.display()
    );
    let text =
        fs::read_to_string(&manifest).with_context(|| format!("reading {}", manifest.display()))?;
    let document: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
    let package = document
        .get("package")
        .and_then(toml::Value::as_table)
        .context("local package manifest must contain [package]")?;
    let org = package
        .get("org")
        .and_then(toml::Value::as_str)
        .context("[package].org must be a string")?;
    let name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .context("[package].name must be a string")?;
    normalize_key(&format!("{org}/{name}"))
}

fn normalize_key(raw: &str) -> Result<String> {
    let key = raw.trim().strip_prefix('@').unwrap_or(raw.trim());
    crate::ops::split_key(key)?;
    Ok(key.to_string())
}

fn key_parts(key: &str) -> Result<(&str, &str)> {
    crate::ops::split_key(key)?;
    key.split_once('/')
        .context("package identity must use org/name")
}

fn canonical_source(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalizing {}", path.display()))?;
    ensure!(
        canonical.is_dir(),
        "{} is not a directory",
        canonical.display()
    );
    Ok(canonical)
}

fn safe_destination(project: &Path, source: &Path, destination: &Path) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .context("local-link destination has no file name")?;
    let parent = destination
        .parent()
        .context("local-link destination has no parent")?;

    let mut existing = parent;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = existing.file_name().with_context(|| {
                    format!(
                        "cannot locate an existing ancestor for local-link destination {}",
                        destination.display()
                    )
                })?;
                missing.push(component.to_os_string());
                existing = existing.parent().with_context(|| {
                    format!(
                        "cannot locate an existing ancestor for local-link destination {}",
                        destination.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading local-link destination ancestor {}", existing.display())
                });
            }
        }
    }

    let mut canonical_parent = existing.canonicalize().with_context(|| {
        format!(
            "canonicalizing local-link destination ancestor {}",
            existing.display()
        )
    })?;
    for component in missing.iter().rev() {
        canonical_parent.push(component);
    }
    let canonical_destination = canonical_parent.join(name);
    ensure!(
        canonical_destination.starts_with(project),
        "local-link destination {} escapes consumer project {}",
        canonical_destination.display(),
        project.display()
    );
    ensure!(
        source != canonical_destination
            && !source.starts_with(&canonical_destination)
            && !canonical_destination.starts_with(source),
        "local-link source {} overlaps destination {}; refusing self-linking or recursive ownership",
        source.display(),
        canonical_destination.display()
    );
    Ok(canonical_destination)
}

fn path_to_utf8(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("{label} is not valid UTF-8: {}", path.display()))
}

fn relative_utf8(project: &Path, path: &Path, label: &str) -> Result<String> {
    let relative = path
        .strip_prefix(project)
        .with_context(|| format!("{label} escapes project {}", project.display()))?;
    path_to_utf8(relative, label)
}

fn checked_project_relative(project: &Path, raw: &str) -> Result<PathBuf> {
    let relative = Path::new(raw);
    ensure!(
        !relative.is_absolute(),
        "managed local-link path must be project-relative: {raw}"
    );
    ensure!(
        !relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)),
        "managed local-link path may not contain `..`: {raw}"
    );
    let path = project.join(relative);
    ensure!(
        path.starts_with(project),
        "managed local-link path escapes consumer project: {raw}"
    );
    Ok(path)
}

fn registration_path(home: &Path, package: &str) -> Result<PathBuf> {
    let (org, name) = key_parts(package)?;
    Ok(links_root(home).join(org).join(format!("{name}.json")))
}

fn consumer_receipt_path(project: &Path, package: &str) -> Result<PathBuf> {
    let (org, name) = key_parts(package)?;
    Ok(project
        .join(CONSUMER_STATE_DIR)
        .join(org)
        .join(format!("{name}.json")))
}

fn backup_relative_path(package: &str, label: &str) -> Result<String> {
    let (org, name) = key_parts(package)?;
    Ok(format!("{CONSUMER_BACKUP_DIR}/{org}/{name}/{label}"))
}

fn links_root(home: &Path) -> PathBuf {
    home.join(LINKS_DIR)
}

fn acquire_registry_lock(home: &Path) -> Result<LockGuard> {
    let root = links_root(home);
    create_private_dir(&root)?;
    let lock_path = root.join(LINKS_LOCK_FILE);
    LockManager::global()
        .acquire_blocking(
            LockRequest::exclusive(&lock_path)
                .operation("local package link registry")
                .class(LockClass::Custom(6))
                .queue_same_process(),
        )
        .context("locking local package link registry")
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(())
}

fn read_json_regular<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading managed state {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "managed state {} must be a regular file",
        path.display()
    );
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

fn write_json_atomic<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let parent = path.parent().context("managed state path has no parent")?;
    create_private_dir(parent)?;

    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(
            metadata.file_type().is_file(),
            "refusing to replace non-regular managed state {}",
            path.display()
        );
    }

    let filename = path
        .file_name()
        .and_then(OsStr::to_str)
        .context("managed state path has no UTF-8 file name")?;
    let temporary = parent.join(format!(".{filename}.{}.tmp", Uuid::new_v4()));
    let result = write_json_file(&temporary, value)
        .and_then(|()| replace_atomic(&temporary, path))
        .with_context(|| format!("committing managed state {}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_json_file<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, value)
        .with_context(|| format!("serializing {}", path.display()))?;
    file.write_all(b"\n")?;
    file.sync_all()
        .with_context(|| format!("syncing {}", path.display()))
}

fn replace_atomic(temporary: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    if fs::symlink_metadata(destination).is_ok() {
        fs::remove_file(destination)
            .with_context(|| format!("removing old state {}", destination.display()))?;
    }
    fs::rename(temporary, destination).with_context(|| {
        format!(
            "renaming {} to {}",
            temporary.display(),
            destination.display()
        )
    })?;
    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing state directory {}", parent.display()))?;
    }
    Ok(())
}

fn create_directory_symlink(target: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("local-link destination has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating local-link parent {}", parent.display()))?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, destination).with_context(|| {
            format!(
                "creating live package link {} -> {}",
                destination.display(),
                target.display()
            )
        })
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, destination).with_context(|| {
            format!(
                "creating live package directory link {} -> {}; Windows may require Developer Mode or symlink privileges",
                destination.display(),
                target.display()
            )
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = target;
        let _ = destination;
        bail!("live local package links are unsupported on this platform")
    }
}

fn is_explicit_path(raw: &str) -> bool {
    let path = Path::new(raw);
    path.is_absolute()
        || raw == "."
        || raw == ".."
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw.starts_with(".\\")
        || raw.starts_with("..\\")
}

fn resolve_cli_path(cwd: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn route(args: &[OsString]) -> Route {
    let Some((command_index, command)) = first_command(args) else {
        return Route::Existing;
    };
    match command.as_str() {
        "link" | "ln" | "unlink" | "links" => Route::Link,
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target))
                if matches!(target.as_str(), "link" | "ln" | "unlink" | "links") =>
            {
                Route::Help {
                    help_index: command_index,
                    target_index,
                }
            }
            _ => Route::Existing,
        },
        _ => Route::Existing,
    }
}

fn first_command(args: &[OsString]) -> Option<(usize, String)> {
    let mut index = 1;
    while index < args.len() {
        let token = args.get(index)?.to_string_lossy();
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
        let token = args.get(index)?.to_string_lossy();
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

fn validate_link_flags(argv: &[String]) -> Result<()> {
    let parser_argv = argv
        .iter()
        .filter(|token| !matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
        .cloned()
        .collect::<Vec<_>>();
    let contract_dir = tempfile::tempdir().context("creating local-link flags2env directory")?;
    let contract_path = contract_dir.path().join(".cli-flags.toml");
    fs::write(&contract_path, LINK_CONTRACT).context("writing embedded local-link contract")?;
    let contract_path = contract_path
        .to_str()
        .context("embedded local-link contract path is not UTF-8")?;
    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed link flags2env audit failed: {error}"))?;
    let parsed = parser
        .parse_structured(&parser_argv, Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed link flags2env parse failed: {error}"))?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "flags2env rejected unknown zed link option(s): {}",
            parsed.unknown_options.join(", ")
        );
    }
    if !parsed.errors.is_empty() {
        bail!(
            "flags2env rejected invalid zed link value(s): {}",
            parsed.errors.join("; ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(root: &Path, org: &str, name: &str) -> Result<()> {
        fs::create_dir_all(root)?;
        fs::write(
            root.join(MANIFEST_FILE),
            format!(
                "[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"0.1.0\"\n"
            ),
        )?;
        Ok(())
    }

    fn symlink_target(path: &Path) -> Result<PathBuf> {
        let raw = fs::read_link(path)?;
        let resolved = if raw.is_absolute() {
            raw
        } else {
            path.parent().context("symlink test path has no parent")?.join(raw)
        };
        Ok(resolved.canonicalize()?)
    }

    fn require_error<T>(result: Result<T>, message: &str) -> Result<anyhow::Error> {
        match result {
            Ok(_) => bail!("{message}"),
            Err(error) => Ok(error),
        }
    }

    #[cfg(unix)]
    #[test]
    fn register_consume_and_unlink_restore_previous_links() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        let old = temp.path().join("old");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        write_manifest(&old, "acme", "widget")?;
        fs::write(source.join("payload.txt"), "one")?;
        fs::write(consumer.join("package.json"), "{}")?;

        let zed_destination = consumer.join(MODULES_DIR).join("acme/widget");
        let node_destination = consumer.join("node_modules/@acme/widget");
        create_directory_symlink(&old, &zed_destination)?;
        create_directory_symlink(&old, &node_destination)?;

        register(&home, &source)?;
        let receipt = consume(&consumer, &home, "acme/widget", LocalLinkAdapter::Auto)?;
        assert_eq!(receipt.projections.len(), 2);
        assert_eq!(symlink_target(&zed_destination)?, source.canonicalize()?);
        assert_eq!(symlink_target(&node_destination)?, source.canonicalize()?);

        fs::write(source.join("payload.txt"), "two")?;
        assert_eq!(fs::read_to_string(zed_destination.join("payload.txt"))?, "two");

        unlink_consumer(&consumer, "acme/widget")?;
        assert_eq!(symlink_target(&zed_destination)?, old.canonicalize()?);
        assert_eq!(symlink_target(&node_destination)?, old.canonicalize()?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn regular_directory_is_backed_up_and_restored() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        let destination = consumer.join(MODULES_DIR).join("acme/widget");
        fs::create_dir_all(&destination)?;
        fs::write(destination.join("old.txt"), "old")?;

        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None)?;
        assert_eq!(symlink_target(&destination)?, source.canonicalize()?);

        unlink_consumer(&consumer, "acme/widget")?;
        assert!(destination.is_dir());
        assert_eq!(fs::read_to_string(destination.join("old.txt"))?, "old");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn link_does_not_mutate_manifest_or_lock() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        let lock = consumer.join(".zpkg.lock");
        fs::write(&lock, "sentinel-lock\n")?;
        let before_manifest = fs::read(consumer.join(MANIFEST_FILE))?;
        let before_lock = fs::read(&lock)?;

        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None)?;

        assert_eq!(fs::read(consumer.join(MANIFEST_FILE))?, before_manifest);
        assert_eq!(fs::read(&lock)?, before_lock);
        Ok(())
    }

    #[test]
    fn stale_registration_identity_is_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        register(&home, &source)?;
        write_manifest(&source, "other", "widget")?;

        let error = require_error(
            consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None),
            "identity drift must be rejected",
        )?;
        assert!(format!("{error:#}").contains("instead of `acme/widget`"));
        Ok(())
    }

    #[test]
    fn bare_names_must_be_unique() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        write_manifest(&left, "one", "widget")?;
        write_manifest(&right, "two", "widget")?;
        register(&home, &left)?;
        register(&home, &right)?;

        let error = require_error(
            resolve_registered_key(&home, "widget"),
            "ambiguous bare name must be rejected",
        )?;
        assert!(format!("{error:#}").contains("ambiguous"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unlink_refuses_a_projection_changed_by_another_tool() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let other = temp.path().join("other");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&other, "acme", "other")?;
        write_manifest(&consumer, "acme", "consumer")?;
        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None)?;
        let destination = consumer.join(MODULES_DIR).join("acme/widget");
        fs::remove_file(&destination)?;
        create_directory_symlink(&other, &destination)?;

        let error = require_error(
            unlink_consumer(&consumer, "acme/widget"),
            "changed projection must be preserved",
        )?;
        assert!(format!("{error:#}").contains("changed by another tool"));
        assert_eq!(symlink_target(&destination)?, other.canonicalize()?);
        Ok(())
    }

    #[test]
    fn npm_style_scoped_key_normalizes_to_zed_identity() -> Result<()> {
        assert_eq!(normalize_key("@acme/widget")?, "acme/widget");
        Ok(())
    }
}
