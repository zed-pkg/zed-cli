use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use anyhow::Result as AnyResult;
use clap::{Args, Command, CommandFactory, Parser, Subcommand, ValueEnum};

mod api;
mod build;
mod error;
mod model;
mod render;

use api::GitHubClient;
use build::{BuildRequest, InventoryBuilder};
use error::{ErrorClass, InventoryError};
use model::Limits;
use render::{render_inventory, write_atomic};

const DEFAULT_API_BASE: &str = "https://api.github.com";
const ROOT_GLOBAL_OPTIONS: &[&str] = &[
    "registry",
    "home",
    "token",
    "auth-url",
    "supabase-url",
    "supabase-key",
];
const ROOT_GLOBAL_FLAGS: &[&str] = &["interactive"];
const ROOT_GLOBAL_OPTIONAL_BOOL: &[&str] = &["git-submodules"];

#[derive(Debug, Parser)]
#[command(
    name = "graph",
    about = "Inspect dependency graphs and source inventories",
    disable_help_subcommand = true
)]
struct GraphCli {
    #[command(subcommand)]
    command: GraphCommand,
}

#[derive(Debug, Subcommand)]
enum GraphCommand {
    /// Inventory dependencies across GitHub repositories or organizations.
    Github(GithubArgs),
}

#[derive(Debug, Args)]
struct GithubArgs {
    /// Explicit GitHub repository in owner/name form; repeatable.
    #[arg(long = "repo", value_name = "OWNER/NAME", action = clap::ArgAction::Append)]
    repositories: Vec<String>,

    /// GitHub organization whose complete repository inventory should be scanned; repeatable.
    #[arg(long = "org", value_name = "OWNER", action = clap::ArgAction::Append)]
    organizations: Vec<String>,

    /// Comma-separated source kinds: zed,git-submodule,nix; repeatable.
    #[arg(long = "include", action = clap::ArgAction::Append)]
    includes: Vec<String>,

    /// Deterministic output representation.
    #[arg(long, value_enum, default_value = "json")]
    format: OutputFormat,

    /// Write atomically to this file; '-' writes to stdout.
    #[arg(long)]
    output: Option<PathBuf>,

    /// GitHub REST API base. Plain HTTP is accepted only on loopback.
    #[arg(long, env = "ZED_PKG_GITHUB_API_BASE", default_value = DEFAULT_API_BASE)]
    api_base: String,

    #[arg(long, env = "ZED_PKG_GRAPH_MAX_REPOSITORIES", default_value_t = 1_000)]
    max_repositories: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_NODES", default_value_t = 20_000)]
    max_nodes: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_EDGES", default_value_t = 40_000)]
    max_edges: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_PINS", default_value_t = 40_000)]
    max_pins: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_REQUESTS", default_value_t = 20_000)]
    max_requests: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_RESPONSE_BYTES", default_value_t = 4 * 1024 * 1024)]
    max_response_bytes: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_TOTAL_RESPONSE_BYTES", default_value_t = 128 * 1024 * 1024)]
    max_total_response_bytes: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_MANIFEST_BYTES", default_value_t = 2 * 1024 * 1024)]
    max_manifest_bytes: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_FIELD_BYTES", default_value_t = 16 * 1024)]
    max_field_bytes: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_TREE_ENTRIES", default_value_t = 50_000)]
    max_tree_entries: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_JSON_DEPTH", default_value_t = 64)]
    max_json_depth: usize,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_SECONDS", default_value_t = 120.0)]
    max_seconds: f64,
    #[arg(long, env = "ZED_PKG_GRAPH_MAX_RETRIES", default_value_t = 2)]
    max_retries: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
    Dot,
    Mermaid,
}

impl OutputFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Dot => "dot",
            Self::Mermaid => "mermaid",
        }
    }
}

pub fn augment_root_command(command: Command) -> Command {
    command.subcommand(GraphCli::command())
}

/// Route the modular command before the monolithic root parser.
///
/// This deliberately mirrors the established `develop`/`gitops` dispatcher
/// contract: unrelated commands return `None`; routed parse/runtime failures
/// remain ordinary command results for the root binary to report consistently.
pub fn dispatch(args: Vec<OsString>) -> Option<AnyResult<i32>> {
    let routed = match route_tokens(&args) {
        Ok(Some(tokens)) => tokens,
        Ok(None) => return None,
        Err(error) => {
            eprintln!("error: {error}");
            return Some(Ok(exit_code_for_error(&error)));
        }
    };
    Some(match GraphCli::try_parse_from(routed) {
        Ok(parsed) => run_cli(parsed),
        Err(error) => {
            let exit_code = error.exit_code();
            match error.print() {
                Ok(()) => Ok(exit_code),
                Err(print_error) => Err(print_error.into()),
            }
        }
    })
}

fn run_cli(parsed: GraphCli) -> AnyResult<i32> {
    let result = match parsed.command {
        GraphCommand::Github(arguments) => run(arguments),
    };
    match result {
        Ok(exit_code) => Ok(exit_code),
        Err(error) => {
            eprintln!("error: {error}");
            Ok(exit_code_for_error(&error))
        }
    }
}

fn run(arguments: GithubArgs) -> Result<i32, InventoryError> {
    let limits = build_limits(&arguments);
    limits.validate()?;

    let token = std::env::var("ZED_PKG_GITHUB_TOKEN")
        .ok()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .filter(|value| !value.trim().is_empty());
    let allow_custom = env_bool("ZED_PKG_GITHUB_ALLOW_TOKEN_TO_API_BASE")?;

    let client = GitHubClient::new(&arguments.api_base, token, allow_custom, limits.clone())?;
    let inventory = InventoryBuilder::new(
        client,
        BuildRequest {
            repositories: arguments.repositories,
            organizations: arguments.organizations,
            includes: arguments.includes,
        },
        limits,
    )?
    .build()?;

    let rendered = render_inventory(&inventory, arguments.format.as_str())?;
    match arguments.output.as_deref() {
        Some(path) if path.as_os_str() == OsStr::new("-") => {
            print!("{rendered}");
        }
        Some(path) => write_atomic(path, &rendered)?,
        None => print!("{rendered}"),
    }

    Ok(if inventory.completeness.inventory == "partial" {
        1
    } else {
        0
    })
}

fn build_limits(arguments: &GithubArgs) -> Limits {
    Limits {
        max_repositories: arguments.max_repositories,
        max_nodes: arguments.max_nodes,
        max_edges: arguments.max_edges,
        max_pins: arguments.max_pins,
        max_requests: arguments.max_requests,
        max_response_bytes: arguments.max_response_bytes,
        max_total_response_bytes: arguments.max_total_response_bytes,
        max_manifest_bytes: arguments.max_manifest_bytes,
        max_field_bytes: arguments.max_field_bytes,
        max_tree_entries: arguments.max_tree_entries,
        max_json_depth: arguments.max_json_depth,
        max_seconds: arguments.max_seconds,
        max_retries: arguments.max_retries,
    }
}

fn route_tokens(args: &[OsString]) -> Result<Option<Vec<OsString>>, InventoryError> {
    let Some((command_index, command)) = first_command(args) else {
        return Ok(None);
    };

    if command == "graph" {
        return Ok(Some(graph_tokens_after(args, command_index)?));
    }

    if command != "help" {
        return Ok(None);
    }

    let Some((target_index, target)) = next_non_option(args, command_index + 1) else {
        return Ok(None);
    };
    if target != "graph" {
        return Ok(None);
    }

    let mut routed = vec![OsString::from("graph")];
    append_graph_arguments(args, target_index + 1, &mut routed)?;
    if !routed
        .iter()
        .any(|value| value == "--help" || value == "-h")
    {
        routed.push(OsString::from("--help"));
    }
    Ok(Some(routed))
}

fn first_command(args: &[OsString]) -> Option<(usize, String)> {
    let mut index = 1;
    while index < args.len() {
        let value = args[index].to_string_lossy();
        if let Some(name) = value.strip_prefix("--") {
            let name = name.split('=').next().unwrap_or(name);
            if ROOT_GLOBAL_OPTIONS.contains(&name) && !value.contains('=') {
                index += 2;
                continue;
            }
            if ROOT_GLOBAL_FLAGS.contains(&name) || ROOT_GLOBAL_OPTIONAL_BOOL.contains(&name) {
                index += 1;
                continue;
            }
            index += 1;
            continue;
        }
        if value.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((index, value.into_owned()));
    }
    None
}

fn graph_tokens_after(
    args: &[OsString],
    command_index: usize,
) -> Result<Vec<OsString>, InventoryError> {
    let mut routed = vec![OsString::from("graph")];
    append_graph_arguments(args, command_index + 1, &mut routed)?;
    Ok(routed)
}

fn append_graph_arguments(
    args: &[OsString],
    start: usize,
    routed: &mut Vec<OsString>,
) -> Result<(), InventoryError> {
    let mut index = start;
    while index < args.len() {
        let value = args[index].to_string_lossy();
        if let Some(name) = value.strip_prefix("--") {
            let name = name.split('=').next().unwrap_or(name);
            if ROOT_GLOBAL_OPTIONS.contains(&name) {
                if !value.contains('=') {
                    if index + 1 >= args.len() {
                        return Err(InventoryError::input(
                            "missing_global_value",
                            format!("--{name} requires a value"),
                        ));
                    }
                    index += 2;
                } else {
                    index += 1;
                }
                continue;
            }
            if ROOT_GLOBAL_FLAGS.contains(&name) || ROOT_GLOBAL_OPTIONAL_BOOL.contains(&name) {
                index += 1;
                continue;
            }
        }
        routed.push(args[index].clone());
        index += 1;
    }
    Ok(())
}

fn next_non_option(args: &[OsString], start: usize) -> Option<(usize, String)> {
    args.iter()
        .enumerate()
        .skip(start)
        .find_map(|(index, value)| {
            let text = value.to_string_lossy();
            (!text.starts_with('-')).then(|| (index, text.into_owned()))
        })
}

fn env_bool(name: &str) -> Result<bool, InventoryError> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(false);
    };
    let normalized = value.to_string_lossy().trim().to_ascii_lowercase();
    match normalized.as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        _ => Err(InventoryError::input(
            "invalid_boolean_environment",
            format!("{name} must be a boolean value"),
        )),
    }
}

pub fn exit_code_for_error(error: &InventoryError) -> i32 {
    match error.class() {
        ErrorClass::Partial => 1,
        ErrorClass::Input => 2,
        ErrorClass::Limit => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn routes_nested_graph_command_with_root_globals() {
        let routed = route_tokens(&args(&[
            "zed",
            "--registry",
            "https://registry.example",
            "graph",
            "github",
            "--org",
            "Acme",
        ]))
        .expect("route")
        .expect("graph route");
        assert_eq!(routed, args(&["graph", "github", "--org", "Acme"]));
    }

    #[test]
    fn routes_help_for_nested_github_command() {
        let routed = route_tokens(&args(&["zed", "help", "graph", "github"]))
            .expect("route")
            .expect("graph help route");
        assert_eq!(routed, args(&["graph", "github", "--help"]));
    }

    #[test]
    fn ignores_unrelated_commands() {
        assert!(
            route_tokens(&args(&["zed", "install"]))
                .expect("route")
                .is_none()
        );
    }

    #[test]
    fn github_command_has_no_credential_flag() {
        let command = GraphCli::command();
        let github = command
            .find_subcommand("github")
            .expect("github subcommand");
        let ids = github
            .get_arguments()
            .map(|argument| argument.get_id().as_str())
            .collect::<Vec<_>>();
        assert!(!ids.contains(&"token"));
        assert!(!ids.contains(&"github-token"));
    }

    #[test]
    fn root_help_contains_graph_command() {
        assert_eq!(GraphCli::command().get_name(), "graph");
    }

    #[test]
    fn error_classes_have_stable_exit_codes() {
        assert_eq!(exit_code_for_error(&InventoryError::partial("x", "y")), 1);
        assert_eq!(exit_code_for_error(&InventoryError::input("x", "y")), 2);
        assert_eq!(exit_code_for_error(&InventoryError::limit("y")), 3);
    }
}
