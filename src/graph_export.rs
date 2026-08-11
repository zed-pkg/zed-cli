//! Immutable package dependency-graph downloads.
//!
//! `zed graph package <org>/<name>@<version>` is a byte-preserving client for
//! the registry graph endpoints. It never resolves a mutable version, rewrites
//! a graph, or treats a convenience projection as lockfile authority.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, CommandFactory, Parser, Subcommand};
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use reqwest::redirect::Policy;
use serde::Serialize;

use crate::cli::Globals;
use crate::config::Config;

const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ALLOWED_BYTES: u64 = 1024 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const GRAPH_DIGEST_HEADER: &str = "x-zpkg-graph-digest";
const GRAPH_AUTHORITATIVE_HEADER: &str = "x-zpkg-graph-authoritative";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteKind {
    Canonical,
    Extended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphFormat {
    Json,
    Yaml,
    Toml,
    Dot,
    Mermaid,
    Json5,
    Xml,
    Csv,
    MessagePack,
    Protobuf,
}

impl GraphFormat {
    const fn name(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Dot => "dot",
            Self::Mermaid => "mermaid",
            Self::Json5 => "json5",
            Self::Xml => "xml",
            Self::Csv => "csv",
            Self::MessagePack => "msgpack",
            Self::Protobuf => "protobuf",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "json" => Self::Json,
            "yaml" | "yml" => Self::Yaml,
            "toml" => Self::Toml,
            "dot" | "graphviz" => Self::Dot,
            "mermaid" | "mmd" => Self::Mermaid,
            "json5" => Self::Json5,
            "xml" => Self::Xml,
            "csv" => Self::Csv,
            "msgpack" | "messagepack" | "mpk" => Self::MessagePack,
            "protobuf" | "proto" | "pb" => Self::Protobuf,
            _ => bail!(
                "unsupported dependency graph format `{value}`; expected json, yaml, toml, dot, mermaid, json5, xml, csv, msgpack, or protobuf"
            ),
        })
    }

    const fn route_kind(self) -> RouteKind {
        match self {
            Self::Json | Self::Yaml | Self::Toml | Self::Dot | Self::Mermaid => {
                RouteKind::Canonical
            }
            Self::Json5 | Self::Xml | Self::Csv | Self::MessagePack | Self::Protobuf => {
                RouteKind::Extended
            }
        }
    }

    const fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Dot => "dot",
            Self::Mermaid => "mmd",
            Self::Json5 => "json5",
            Self::Xml => "xml",
            Self::Csv => "csv",
            Self::MessagePack => "msgpack",
            Self::Protobuf => "pb",
        }
    }

    const fn media_type(self) -> &'static str {
        match self {
            Self::Json => "application/vnd.zpkg.dependency-graph.v1+json",
            Self::Yaml => "application/vnd.zpkg.dependency-graph.v1+yaml",
            Self::Toml => "application/vnd.zpkg.dependency-graph.v1+toml",
            Self::Dot => "text/vnd.graphviz; charset=utf-8",
            Self::Mermaid => "text/vnd.mermaid; charset=utf-8",
            Self::Json5 => "application/vnd.zpkg.dependency-graph.v1+json5",
            Self::Xml => "application/vnd.zpkg.dependency-graph.v1+xml",
            Self::Csv => "text/csv; charset=utf-8",
            Self::MessagePack => "application/vnd.zpkg.dependency-graph.v1+msgpack",
            Self::Protobuf => "application/vnd.zpkg.dependency-graph.v1+protobuf",
        }
    }

    const fn authoritative(self) -> bool {
        matches!(
            self,
            Self::Json
                | Self::Yaml
                | Self::Toml
                | Self::Json5
                | Self::Xml
                | Self::MessagePack
                | Self::Protobuf
        )
    }

    const fn binary(self) -> bool {
        matches!(self, Self::MessagePack | Self::Protobuf)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageCoordinate {
    org: String,
    name: String,
    version: String,
}

impl PackageCoordinate {
    fn parse(value: &str) -> Result<Self> {
        let (package, version) = value.rsplit_once('@').with_context(|| {
            format!("package coordinate `{value}` must include an exact @version")
        })?;
        let mut package_parts = package.split('/');
        let org = package_parts.next().unwrap_or_default();
        let name = package_parts.next().unwrap_or_default();
        ensure!(
            package_parts.next().is_none(),
            "package coordinate `{value}` must be exactly org/name@version"
        );
        ensure!(
            zed_interfaces::manifest::is_slug(org),
            "package organization `{org}` is not a valid lowercase slug"
        );
        ensure!(
            zed_interfaces::manifest::is_slug(name),
            "package name `{name}` is not a valid lowercase slug"
        );
        validate_version_segment(version)?;
        Ok(Self {
            org: org.to_string(),
            name: name.to_string(),
            version: version.to_string(),
        })
    }

    fn display(&self) -> String {
        format!("{}/{}@{}", self.org, self.name, self.version)
    }

    fn suggested_filename(&self, format: GraphFormat) -> String {
        let safe_version: String = self
            .version
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || matches!(character, '.' | '+' | '-') {
                    character
                } else {
                    '_'
                }
            })
            .collect();
        format!(
            "{}_{}_{}.dependency-graph.{}",
            self.org,
            self.name,
            safe_version,
            format.extension()
        )
    }
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

struct DownloadedGraph {
    body: Vec<u8>,
    not_modified: bool,
    authoritative: bool,
    etag: Option<String>,
    graph_digest: Option<String>,
    content_type: Option<String>,
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

    let url = graph_url(&config.registry, &coordinate, format)?;
    let client = Client::builder()
        .redirect(Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building dependency graph HTTP client")?;
    let mut request = client.get(url).header(ACCEPT, format.media_type());
    if let Some(token) = config.resolve_token()? {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(etag) = options.etag.as_deref().filter(|value| !value.is_empty()) {
        request = request.header(IF_NONE_MATCH, etag);
    }

    let response = request
        .send()
        .context("requesting immutable package dependency graph")?;
    let downloaded = consume_response(response, format, options.max_bytes)?;
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

fn graph_url(base: &str, coordinate: &PackageCoordinate, format: GraphFormat) -> Result<Url> {
    let normalized = format!("{}/", base.trim_end_matches('/'));
    let mut url = Url::parse(&normalized)
        .with_context(|| format!("registry URL `{base}` is not a valid absolute URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "dependency graph downloads require an HTTP(S) registry URL"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "registry URL may not embed credentials; use --token or ZED_PKG_TOKEN"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "registry URL may not contain a query or fragment"
    );
    let mut path = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("registry URL cannot be used as a base URL"))?;
    path.pop_if_empty();
    path.extend([
        "v1",
        "packages",
        &coordinate.org,
        &coordinate.name,
        "versions",
        &coordinate.version,
        "dependency-graph",
    ]);
    match format.route_kind() {
        RouteKind::Canonical => {
            drop(path);
            url.query_pairs_mut()
                .append_pair("view", "declared")
                .append_pair("format", format.name());
        }
        RouteKind::Extended => {
            path.extend(["export", format.name()]);
        }
    }
    Ok(url)
}

fn consume_response(mut response: Response, format: GraphFormat, max_bytes: u64) -> Result<DownloadedGraph> {
    let status = response.status();
    let etag = header_value(response.headers(), ETAG.as_str())?;
    let graph_digest = header_value(response.headers(), GRAPH_DIGEST_HEADER)?;
    let content_type = header_value(response.headers(), CONTENT_TYPE.as_str())?;
    let authoritative = match header_value(response.headers(), GRAPH_AUTHORITATIVE_HEADER)? {
        Some(value) if value.eq_ignore_ascii_case("true") => true,
        Some(value) if value.eq_ignore_ascii_case("false") => false,
        Some(value) => bail!(
            "registry returned invalid {GRAPH_AUTHORITATIVE_HEADER} header `{value}`"
        ),
        None => format.authoritative(),
    };

    if status == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(DownloadedGraph {
            body: Vec::new(),
            not_modified: true,
            authoritative,
            etag,
            graph_digest,
            content_type,
        });
    }
    ensure!(
        status == reqwest::StatusCode::OK,
        "dependency graph request failed with HTTP {status}"
    );
    if let Some(length) = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
    {
        ensure!(
            length <= max_bytes,
            "dependency graph body exceeds the {max_bytes}-byte client limit"
        );
    }

    let limit = max_bytes
        .checked_add(1)
        .context("dependency graph byte limit overflow")?;
    let mut body = Vec::new();
    response
        .take(limit)
        .read_to_end(&mut body)
        .context("reading dependency graph response")?;
    ensure!(
        body.len() as u64 <= max_bytes,
        "dependency graph body exceeds the {max_bytes}-byte client limit"
    );
    Ok(DownloadedGraph {
        body,
        not_modified: false,
        authoritative,
        etag,
        graph_digest,
        content_type,
    })
}

fn header_value(headers: &reqwest::header::HeaderMap, name: &str) -> Result<Option<String>> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .with_context(|| format!("registry returned non-ASCII `{name}` header"))
        })
        .transpose()
}

fn write_body(output: Option<&Path>, format: GraphFormat, body: &[u8]) -> Result<String> {
    match output {
        None => {
            ensure!(!format.binary(), "binary graph output requires --output");
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(body)
                .context("writing dependency graph to stdout")?;
            stdout.flush().context("flushing dependency graph stdout")?;
            Ok("stdout".to_string())
        }
        Some(path) if path == Path::new("-") => {
            let mut stdout = io::stdout().lock();
            stdout
                .write_all(body)
                .context("writing dependency graph to stdout")?;
            stdout.flush().context("flushing dependency graph stdout")?;
            Ok("stdout".to_string())
        }
        Some(path) => write_atomic_file(path, body),
    }
}

fn write_atomic_file(path: &Path, body: &[u8]) -> Result<String> {
    ensure!(
        !path.as_os_str().is_empty(),
        "dependency graph output path may not be empty"
    );
    ensure!(
        fs::symlink_metadata(path).is_err(),
        "dependency graph output already exists: {}",
        path.display()
    );
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata = fs::metadata(parent)
        .with_context(|| format!("reading output directory {}", parent.display()))?;
    ensure!(
        metadata.is_dir(),
        "dependency graph output parent is not a directory: {}",
        parent.display()
    );

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating atomic output beside {}", path.display()))?;
    temporary
        .write_all(body)
        .with_context(|| format!("writing temporary graph output for {}", path.display()))?;
    temporary
        .as_file_mut()
        .sync_all()
        .with_context(|| format!("syncing temporary graph output for {}", path.display()))?;
    temporary.persist_noclobber(path).map_err(|error| {
        anyhow::anyhow!(
            "publishing dependency graph output {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(path.display().to_string())
}

fn validate_version_segment(value: &str) -> Result<()> {
    ensure!(!value.is_empty(), "package version may not be empty");
    ensure!(
        value != "." && value != "..",
        "package version may not be a dot segment"
    );
    ensure!(
        !value.chars().any(|character| {
            character == '/' || character == '\\' || character.is_control()
        }),
        "package version contains a path separator or control character"
    );
    Ok(())
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

    fn coordinate() -> PackageCoordinate {
        PackageCoordinate::parse("acme/http-kit@2.0.0-beta.1+build.7").unwrap()
    }

    #[test]
    fn aliases_map_to_stable_names_and_semantics() {
        assert_eq!(GraphFormat::parse("YML").unwrap(), GraphFormat::Yaml);
        assert_eq!(GraphFormat::parse("graphviz").unwrap(), GraphFormat::Dot);
        assert_eq!(
            GraphFormat::parse("messagepack").unwrap(),
            GraphFormat::MessagePack
        );
        assert_eq!(
            GraphFormat::parse("PB").unwrap(),
            GraphFormat::Protobuf
        );
        assert!(!GraphFormat::Csv.authoritative());
        assert!(!GraphFormat::Dot.authoritative());
        assert!(GraphFormat::Xml.authoritative());
        assert!(GraphFormat::MessagePack.binary());
        assert!(GraphFormat::parse("pickle").is_err());
    }

    #[test]
    fn exact_coordinate_parser_rejects_requirements_and_traversal() {
        assert_eq!(coordinate().org, "acme");
        assert_eq!(coordinate().name, "http-kit");
        assert!(PackageCoordinate::parse("acme/http-kit").is_err());
        assert!(PackageCoordinate::parse("acme/http-kit@../secret").is_err());
        assert!(PackageCoordinate::parse("acme/nested/name@1.0.0").is_err());
        assert!(PackageCoordinate::parse("Acme/http-kit@1.0.0").is_err());
    }

    #[test]
    fn canonical_and_extended_routes_are_distinct_and_encoded() {
        let canonical = graph_url(
            "https://registry.example/internal/",
            &coordinate(),
            GraphFormat::Yaml,
        )
        .unwrap();
        assert_eq!(
            canonical.path(),
            "/internal/v1/packages/acme/http-kit/versions/2.0.0-beta.1+build.7/dependency-graph"
        );
        assert_eq!(canonical.query(), Some("view=declared&format=yaml"));

        let binary = graph_url(
            "https://registry.example/internal/",
            &coordinate(),
            GraphFormat::Protobuf,
        )
        .unwrap();
        assert_eq!(
            binary.path(),
            "/internal/v1/packages/acme/http-kit/versions/2.0.0-beta.1+build.7/dependency-graph/export/protobuf"
        );
        assert!(binary.query().is_none());
    }

    #[test]
    fn registry_base_rejects_credentials_queries_and_non_http_schemes() {
        assert!(
            graph_url(
                "https://user:secret@registry.example",
                &coordinate(),
                GraphFormat::Json
            )
            .is_err()
        );
        assert!(
            graph_url(
                "https://registry.example?token=secret",
                &coordinate(),
                GraphFormat::Json
            )
            .is_err()
        );
        assert!(graph_url("file:///tmp/registry", &coordinate(), GraphFormat::Json).is_err());
    }

    #[test]
    fn suggested_filenames_are_deterministic_and_binary_safe() {
        assert_eq!(
            coordinate().suggested_filename(GraphFormat::Protobuf),
            "acme_http-kit_2.0.0-beta.1+build.7.dependency-graph.pb"
        );
    }

    #[test]
    fn route_detects_graph_and_help_without_stealing_existing_commands() {
        let argv = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            route(&argv(&["zed", "graph", "package", "acme/pkg@1.0.0"])),
            Route::Graph
        );
        assert_eq!(
            route(&argv(&["zed", "--registry", "https://r", "help", "graph", "package"])),
            Route::GraphHelp { help_index: 3 }
        );
        assert_eq!(route(&argv(&["zed", "task", "graph"])), Route::Existing);
    }

    #[test]
    fn atomic_output_refuses_existing_paths() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("graph.json");
        assert_eq!(
            write_atomic_file(&output, b"graph").unwrap(),
            output.display().to_string()
        );
        assert_eq!(fs::read(&output).unwrap(), b"graph");
        assert!(write_atomic_file(&output, b"replacement").is_err());
        assert_eq!(fs::read(&output).unwrap(), b"graph");
    }
}
