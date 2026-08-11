//! Immutable package dependency-graph downloads.
//!
//! `zed graph package <org>/<name>@<version>` is a byte-preserving client for
//! the registry graph endpoints. It never resolves a mutable version, rewrites
//! a graph, or treats a convenience projection as lockfile authority.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;

use crate::cli::Globals;
use crate::config::Config;

mod coordinate;
mod download;
mod format;

use coordinate::PackageCoordinate;
use download::{DownloadRequest, download, write_body};
use format::GraphFormat;

const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ALLOWED_BYTES: u64 = 1024 * 1024 * 1024;
const DOWNLOAD_SCHEMA: &str = "zed.graph-package-download/v1";

#[derive(Debug, Clone, Args)]
pub struct PackageGraphArgs {
    /// Exact immutable package coordinate (`org/name@version`).
    #[arg(value_name = "ORG/NAME@VERSION")]
    pub package: String,

    /// Download representation. Aliases include yml, graphviz, mmd,
    /// messagepack, mpk, proto, and pb.
    #[arg(long, env = "ZED_PKG_GRAPH_FORMAT", default_value = "json")]
    pub format: String,

    /// Output path. Text formats default to stdout. Binary formats require a
    /// path or an explicit `-` to acknowledge binary stdout.
    #[arg(
        long,
        short = 'o',
        env = "ZED_PKG_GRAPH_OUTPUT",
        value_name = "PATH"
    )]
    pub output: Option<PathBuf>,

    /// Send a strong ETag with `If-None-Match`; a 304 leaves output untouched.
    #[arg(long, env = "ZED_PKG_GRAPH_ETAG", value_name = "ETAG")]
    pub etag: Option<String>,

    /// Maximum decoded response bytes accepted by this process.
    #[arg(
        long,
        env = "ZED_PKG_GRAPH_MAX_BYTES",
        default_value_t = DEFAULT_MAX_BYTES,
        value_name = "BYTES"
    )]
    pub max_bytes: u64,

    /// Emit deterministic response metadata as one JSON object on stderr.
    #[arg(long, env = "ZED_PKG_GRAPH_METADATA_JSON")]
    pub metadata_json: bool,
}

#[derive(Debug, Clone, Args)]
struct GraphArgs {
    #[command(subcommand)]
    command: GraphSubcommand,
}

#[derive(Debug, Clone, Subcommand)]
enum GraphSubcommand {
    /// Download one immutable package-version dependency graph.
    Package(PackageGraphArgs),
}

#[derive(Debug, Parser)]
#[command(
    name = "zed",
    version,
    about = "zed: the universal package manager backed by the VCS hosts you already use"
)]
struct GraphCli {
    #[command(flatten)]
    globals: Globals,

    #[command(subcommand)]
    command: GraphCommand,
}

#[derive(Debug, Subcommand)]
enum GraphCommand {
    /// Inspect and export dependency graphs.
    Graph(GraphArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    Graph,
    GraphHelp { help_index: usize },
    Existing,
}

#[derive(Debug, Serialize)]
struct DownloadMetadata {
    schema: &'static str,
    package: String,
    format: &'static str,
    authoritative: bool,
    not_modified: bool,
    bytes: usize,
    etag: Option<String>,
    graph_digest: Option<String>,
    content_type: Option<String>,
    suggested_filename: String,
    output: String,
}

/// Route only `zed graph ...`; established commands remain on the ordinary
/// CLI parser. This modular boundary leaves `zed graph github` available as a
/// sibling command without coupling package downloads to GitHub inventory.
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    match route(&args) {
        Route::Graph => Some(run_cli(args)),
        Route::GraphHelp { help_index } => {
            let mut rewritten = args;
            rewritten.remove(help_index);
            rewritten.push(OsString::from("--help"));
            Some(run_cli(rewritten))
        }
        Route::Existing => None,
    }
}

/// Add the graph namespace and immutable package downloader to root help and
/// shell completion generation.
pub fn augment_root_command(command: clap::Command) -> clap::Command {
    if command
        .get_subcommands()
        .any(|subcommand| subcommand.get_name() == "graph")
    {
        return command;
    }
    let package = <PackageGraphArgs as Args>::augment_args(
        clap::Command::new("package")
            .about("Download one immutable package-version dependency graph"),
    );
    command.subcommand(
        clap::Command::new("graph")
            .about("Inspect and export dependency graphs")
            .subcommand_required(true)
            .arg_required_else_help(true)
            .subcommand(package),
    )
}

fn run_cli(args: Vec<OsString>) -> Result<i32> {
    let cli = match GraphCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            error.print().context("printing zed graph argument error")?;
            return Ok(code);
        }
    };
    let config = Config::from_globals(&cli.globals)?;
    match cli.command {
        GraphCommand::Graph(GraphArgs {
            command: GraphSubcommand::Package(options),
        }) => run_package(&config, options),
    }
}

fn run_package(config: &Config, options: PackageGraphArgs) -> Result<i32> {
    ensure!(
        options.max_bytes > 0 && options.max_bytes <= MAX_ALLOWED_BYTES,
        "--max-bytes must be between 1 and {MAX_ALLOWED_BYTES}"
    );
    let coordinate = PackageCoordinate::parse(&options.package)?;
    let format = GraphFormat::parse(&options.format)?;
    if format.binary() && options.output.is_none() {
        bail!(
            "{} is binary; pass --output <path> or --output - to explicitly write binary stdout",
            format.name()
        );
    }

    let token = config.resolve_token()?;
    let downloaded = download(DownloadRequest {
        registry: &config.registry,
        token: token.as_deref(),
        coordinate: &coordinate,
        format,
        etag: options.etag.as_deref(),
        max_bytes: options.max_bytes,
    })?;
    let output = if downloaded.not_modified {
        "unchanged".to_string()
    } else {
        write_body(options.output.as_deref(), format, &downloaded.body)?
    };
    let metadata = DownloadMetadata {
        schema: DOWNLOAD_SCHEMA,
        package: coordinate.display(),
        format: format.name(),
        authoritative: downloaded.authoritative,
        not_modified: downloaded.not_modified,
        bytes: downloaded.body.len(),
        etag: downloaded.etag,
        graph_digest: downloaded.graph_digest,
        content_type: downloaded.content_type,
        suggested_filename: coordinate.suggested_filename(format),
        output,
    };
    if options.metadata_json {
        eprintln!(
            "{}",
            serde_json::to_string(&metadata).context("serializing graph download metadata")?
        );
    } else if metadata.not_modified {
        eprintln!("dependency graph not modified: {}", metadata.package);
    }
    Ok(0)
}

fn route(args: &[OsString]) -> Route {
    let Some((command_index, command)) = first_command(args) else {
        return Route::Existing;
    };
    match command.as_str() {
        "graph" => Route::Graph,
        "help" => match next_positional(args, command_index + 1) {
            Some((_target_index, target)) if target == "graph" => Route::GraphHelp {
                help_index: command_index,
            },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_detects_graph_and_help_without_stealing_existing_commands() {
        let argv = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            route(&argv(&["zed", "graph", "package", "acme/pkg@1.0.0"])),
            Route::Graph
        );
        assert_eq!(
            route(&argv(&[
                "zed",
                "--registry",
                "https://r",
                "help",
                "graph",
                "package"
            ])),
            Route::GraphHelp { help_index: 3 }
        );
        assert_eq!(route(&argv(&["zed", "task", "graph"])), Route::Existing);
    }
}
