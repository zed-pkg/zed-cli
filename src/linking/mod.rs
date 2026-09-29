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
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use flags2env::BundledFlags2Env;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zed_interfaces::paths::{MANIFEST_FILE, MODULES_DIR};
use zed_lock::{LockClass, LockGuard, LockManager, LockRequest};

use crate::cli::Globals;
use crate::config::Config;

const LINK_CONTRACT: &str = include_str!("../../.link-cli-flags.toml");
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
    #[arg(long, value_enum, env = "ZED_PKG_LINK_ADAPTER", default_value = "auto")]
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
            let receipt = consume(&cwd_canonical, home, &registration.package, options.adapter)?;
            print_linked(&receipt);
            Ok(0)
        }
        Some(target) => {
            let receipt = consume(cwd, home, target, options.adapter)?;
            print_linked(&receipt);
            Ok(0)
        }
    }
}

fn print_linked(receipt: &ConsumerReceipt) {
    println!(
        "linked {} -> {} ({} projection{})",
        receipt.package,
        receipt.source,
        receipt.projections.len(),
        if receipt.projections.len() == 1 {
            ""
        } else {
            "s"
        }
    );
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
            let package = resolve_consumer_key(cwd, home, target)?;
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

include!("storage_a.inc.rs");
include!("storage_b.inc.rs");
include!("registry.inc.rs");
include!("validation_a.inc.rs");
include!("validation_b.inc.rs");
include!("cli.inc.rs");
include!("tests.inc.rs");
