//! Immutable package dependency-graph downloads.
//!
//! `zed graph package <org>/<name>@<version>` is a byte-preserving client for
//! the registry graph endpoints. It never resolves a mutable version, rewrites
//! a graph, or treats a convenience projection as lockfile authority.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use flags2env::BundledFlags2Env;
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
const GRAPH_CONTRACT: &str = include_str!("../.graph-cli-flags.toml");

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
    #[arg(long, short = 'o', env = "ZED_PKG_GRAPH_OUTPUT", value_name = "PATH")]
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
    let string_args = utf8_args(&args)?;
    normalize_boolean_environment()?;
    validate_graph_flags(&string_args)?;

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

fn validate_graph_flags(argv: &[String]) -> Result<()> {
    let parser_argv = argv
        .iter()
        .filter(|token| !matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
        .cloned()
        .collect::<Vec<_>>();
    let parsed = parse_embedded(&parser_argv)?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "flags2env rejected unknown zed graph option(s): {}",
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
            "flags2env rejected invalid zed graph value(s): {}",
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
    let contract_dir = tempfile::tempdir().context("creating zed graph flags2env directory")?;
    let contract_path = contract_dir.path().join(".cli-flags.toml");
    fs::write(&contract_path, GRAPH_CONTRACT).context("writing embedded zed graph contract")?;
    let contract_path = contract_path
        .to_str()
        .context("embedded zed graph contract path is not valid UTF-8")?;

    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed graph flags2env audit failed: {error}"))?;
    parser
        .parse_structured(argv, Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed graph flags2env parse failed: {error}"))
}

fn normalize_boolean_environment() -> Result<()> {
    for key in [
        "ZED_PKG_INTERACTIVE",
        "ZED_PKG_GIT_SUBMODULES",
        "ZED_PKG_NO_MIRRORS",
        "ZED_PKG_TRUST_MIRROR_METADATA",
        "ZED_PKG_SOURCE_FALLBACK",
        "ZED_PKG_GRAPH_METADATA_JSON",
    ] {
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
            // SAFETY: modular graph dispatch runs at process startup before
            // worker threads, matching the existing fetch/develop boundary.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn string_argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

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

    #[test]
    fn embedded_graph_contract_is_fail_closed_and_accepts_public_options() {
        let parsed = parse_embedded(&string_argv(&[
            "zed",
            "graph",
            "package",
            "acme/pkg@1.0.0",
            "--format",
            "json",
            "--output",
            "graph.json",
            "--etag",
            "\"abc\"",
            "--max-bytes",
            "4096",
            "--metadata-json",
        ]))
        .expect("graph flags contract should parse its public command surface");
        assert!(parsed.unknown_options.is_empty());
        assert!(parsed.errors.is_empty());
    }

    #[test]
    fn embedded_graph_contract_rejects_unknown_options() {
        let parsed = parse_embedded(&string_argv(&[
            "zed",
            "graph",
            "package",
            "acme/pkg@1.0.0",
            "--not-a-graph-option",
        ]))
        .expect("flags2env should return structured rejection evidence");
        assert!(!parsed.unknown_options.is_empty() || !parsed.errors.is_empty());
    }
}
