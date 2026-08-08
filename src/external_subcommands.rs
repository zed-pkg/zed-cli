//! Safe dispatch for separately installed `zed-*` command extensions.
//!
//! Built-in commands always win. External commands are resolved beside the
//! running `zed` executable first and then from absolute `PATH` entries. The
//! dispatcher never invokes a shell or searches the current working directory
//! implicitly.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitStatus};

use anyhow::{Context, Result, anyhow};
use clap::{Arg, ArgAction, Command as ClapCommand};

const EXTERNAL_PREFIX: &str = "zed-";
const KNOWN_EXTERNAL_COMMAND: &str = "gitops";
const EXTERNAL_COMMAND_ENV: &str = "ZED_EXTERNAL_SUBCOMMAND";

const ROOT_VALUE_OPTIONS: &[(&str, &str)] = &[
    ("--registry", "ZED_PKG_REGISTRY"),
    ("--home", "ZED_PKG_HOME"),
    ("--token", "ZED_PKG_TOKEN"),
    ("--auth-url", "ZED_PKG_AUTH_URL"),
    ("--supabase-url", "ZED_PKG_SUPABASE_URL"),
    ("--supabase-key", "ZED_PKG_SUPABASE_KEY"),
    ("--global-bin-dir", "ZED_PKG_GLOBAL_BIN_DIR"),
];

const ROOT_BOOLEAN_OPTIONS: &[(&str, &str)] = &[
    ("--interactive", "ZED_PKG_INTERACTIVE"),
    ("--git-submodules", "ZED_PKG_GIT_SUBMODULES"),
];

type ExternalEnvironment = Vec<(OsString, OsString)>;
type ParsedExternalArguments = (Vec<OsString>, ExternalEnvironment);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalRoute {
    name: String,
    arguments: Vec<OsString>,
    environment: ExternalEnvironment,
}

/// Dispatch a root command to `zed-<command>` when it is not built in.
///
/// Returning `None` leaves the established typed root parser in control.
/// Returning `Some` means an external command was selected, including a
/// fail-closed error when the documented `gitops` executable is absent.
pub fn dispatch(args: Vec<OsString>) -> Option<Result<i32>> {
    let route = external_route(&args)?;
    if is_builtin_name(&route.name) {
        return None;
    }

    let executable = resolve_external(&route.name);
    match executable {
        Some(executable) => Some(run_external(&executable, &route)),
        None if route.name == KNOWN_EXTERNAL_COMMAND => Some(Err(anyhow!(
            "external subcommand `gitops` requires `zed-gitops` beside the `zed` executable or in an absolute PATH directory"
        ))),
        None => None,
    }
}

/// Add the supported external GitOps contract to root help and generated
/// completions. Runtime execution still resolves the separately installed
/// `zed-gitops` executable.
pub fn augment_root_command(command: ClapCommand) -> ClapCommand {
    if command.find_subcommand(KNOWN_EXTERNAL_COMMAND).is_some() {
        return command;
    }

    let validate = ClapCommand::new("validate")
        .about("Validate a repository-owned exact-pin GitOps application catalog")
        .arg(
            Arg::new("root")
                .long("root")
                .value_name("PATH")
                .default_value(".")
                .help("Superproject root containing .gitmodules and the Git index"),
        )
        .arg(
            Arg::new("catalog")
                .long("catalog")
                .value_name("PATH")
                .default_value("catalog/gitops/apps")
                .help("Repository-relative GitOpsApplication catalog directory"),
        )
        .arg(
            Arg::new("format")
                .long("format")
                .value_name("FORMAT")
                .value_parser(["human", "json", "sarif"])
                .default_value("human")
                .help("Diagnostic output format"),
        )
        .arg(
            Arg::new("strict")
                .long("strict")
                .action(ArgAction::SetTrue)
                .help("Reject unknown fields in catalog objects"),
        )
        .arg(
            Arg::new("offline")
                .long("offline")
                .action(ArgAction::SetTrue)
                .help("Use only local evidence; required until online checks are implemented"),
        );

    command.subcommand(
        ClapCommand::new(KNOWN_EXTERNAL_COMMAND)
            .about("Validate GitOps composition through the external zed-gitops executable")
            .after_help("Install `zed` and `zed-gitops` into the same bin directory, or place `zed-gitops` in an absolute PATH directory.")
            .subcommand_required(true)
            .subcommand(validate),
    )
}

fn external_route(args: &[OsString]) -> Option<ExternalRoute> {
    let mut index = 1;
    let mut environment = Vec::new();

    while index < args.len() {
        let token = args[index].to_str()?;
        if matches!(token, "--" | "--help" | "-h" | "--version" | "-V") {
            return None;
        }

        if let Some((key, inline)) = root_value_option(token) {
            let (value, consumed) = match inline {
                Some(value) if !value.is_empty() => (OsString::from(value), 1),
                Some(_) => return None,
                None => {
                    let value = args.get(index + 1)?.clone();
                    if value.is_empty() {
                        return None;
                    }
                    (value, 2)
                }
            };
            environment.push((OsString::from(key), value));
            index += consumed;
            continue;
        }

        if let Some((key, value)) = root_boolean_option(token) {
            environment.push((OsString::from(key), OsString::from(value)));
            index += 1;
            continue;
        }

        if token.starts_with('-') {
            return None;
        }

        if token == "help" {
            let name = args.get(index + 1)?.to_str()?;
            if !valid_external_name(name) {
                return None;
            }
            let (mut arguments, trailing_environment) = extract_root_options(&args[index + 2..])?;
            environment.extend(trailing_environment);
            if !arguments
                .iter()
                .any(|argument| argument == OsStr::new("--help") || argument == OsStr::new("-h"))
            {
                arguments.push(OsString::from("--help"));
            }
            return Some(ExternalRoute {
                name: name.to_owned(),
                arguments,
                environment,
            });
        }

        if !valid_external_name(token) {
            return None;
        }
        let (arguments, trailing_environment) = extract_root_options(&args[index + 1..])?;
        environment.extend(trailing_environment);
        return Some(ExternalRoute {
            name: token.to_owned(),
            arguments,
            environment,
        });
    }

    None
}

fn extract_root_options(args: &[OsString]) -> Option<ParsedExternalArguments> {
    let mut arguments = Vec::new();
    let mut environment = Vec::new();
    let mut index = 0;

    while index < args.len() {
        let Some(token) = args[index].to_str() else {
            arguments.push(args[index].clone());
            index += 1;
            continue;
        };
        if token == "--" {
            arguments.extend_from_slice(&args[index..]);
            break;
        }

        if let Some((key, inline)) = root_value_option(token) {
            let (value, consumed) = match inline {
                Some(value) if !value.is_empty() => (OsString::from(value), 1),
                Some(_) => return None,
                None => {
                    let value = args.get(index + 1)?.clone();
                    if value.is_empty() {
                        return None;
                    }
                    (value, 2)
                }
            };
            environment.push((OsString::from(key), value));
            index += consumed;
            continue;
        }

        if let Some((key, value)) = root_boolean_option(token) {
            environment.push((OsString::from(key), OsString::from(value)));
            index += 1;
            continue;
        }
        if is_root_boolean_spelling(token) {
            return None;
        }

        arguments.push(args[index].clone());
        index += 1;
    }

    Some((arguments, environment))
}

fn is_root_boolean_spelling(token: &str) -> bool {
    ROOT_BOOLEAN_OPTIONS.iter().any(|(option, _)| {
        token == *option
            || token == format!("--no-{}", option.trim_start_matches('-'))
            || token
                .strip_prefix(option)
                .is_some_and(|tail| tail.starts_with('='))
    })
}

fn root_value_option(token: &str) -> Option<(&'static str, Option<&str>)> {
    ROOT_VALUE_OPTIONS.iter().find_map(|(option, key)| {
        if token == *option {
            Some((*key, None))
        } else {
            token
                .strip_prefix(option)
                .and_then(|tail| tail.strip_prefix('='))
                .map(|value| (*key, Some(value)))
        }
    })
}

fn root_boolean_option(token: &str) -> Option<(&'static str, &'static str)> {
    ROOT_BOOLEAN_OPTIONS.iter().find_map(|(option, key)| {
        if token == *option {
            return Some((*key, "true"));
        }
        let negative = format!("--no-{}", option.trim_start_matches('-'));
        if token == negative {
            return Some((*key, "false"));
        }
        token
            .strip_prefix(option)
            .and_then(|tail| tail.strip_prefix('='))
            .and_then(normalize_boolean)
            .map(|value| (*key, value))
    })
}

fn normalize_boolean(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some("true"),
        "false" | "0" | "no" | "off" => Some("false"),
        _ => None,
    }
}

fn valid_external_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    name.len() <= 64
        && first.is_ascii_alphanumeric()
        && characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn is_builtin_name(name: &str) -> bool {
    crate::completion::built_in_root_command()
        .find_subcommand(name)
        .is_some()
}

fn resolve_external(name: &str) -> Option<PathBuf> {
    let sibling = env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(Path::to_path_buf));
    let path = env::var_os("PATH");
    resolve_in_locations(name, sibling.as_deref(), path.as_deref())
}

fn resolve_in_locations(
    name: &str,
    sibling: Option<&Path>,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    let stem = format!("{EXTERNAL_PREFIX}{name}");

    if let Some(executable) = sibling
        .filter(|directory| directory.is_absolute())
        .and_then(|directory| executable_in(directory, &stem))
    {
        return Some(executable);
    }

    let path = path?;
    for directory in env::split_paths(path) {
        if !directory.is_absolute() {
            continue;
        }
        if let Some(executable) = executable_in(&directory, &stem) {
            return Some(executable);
        }
    }
    None
}

fn executable_in(directory: &Path, stem: &str) -> Option<PathBuf> {
    executable_candidates(directory, stem)
        .into_iter()
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(windows)]
fn executable_candidates(directory: &Path, stem: &str) -> Vec<PathBuf> {
    vec![directory.join(format!("{stem}.exe")), directory.join(stem)]
}

#[cfg(not(windows))]
fn executable_candidates(directory: &Path, stem: &str) -> Vec<PathBuf> {
    vec![directory.join(stem)]
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn run_external(executable: &Path, route: &ExternalRoute) -> Result<i32> {
    let mut command = ProcessCommand::new(executable);
    command.args(&route.arguments);
    for (key, value) in &route.environment {
        command.env(key, value);
    }
    command.env(EXTERNAL_COMMAND_ENV, &route.name);

    let status = command.status().with_context(|| {
        format!(
            "running external subcommand `{}` through {}",
            route.name,
            executable.display()
        )
    })?;
    Ok(status_code(status))
}

fn status_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|signal| 128 + signal).unwrap_or(1)
    }
    #[cfg(not(unix))]
    {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn routes_plugin_arguments_without_shell_rewriting() {
        let route = external_route(&os_args(&[
            "zed",
            "gitops",
            "validate",
            "--root",
            "workspace with spaces",
            "--offline",
        ]))
        .expect("external route");
        assert_eq!(route.name, "gitops");
        assert_eq!(
            route.arguments,
            os_args(&["validate", "--root", "workspace with spaces", "--offline"])
        );
    }

    #[test]
    fn root_options_before_plugin_become_child_environment() {
        let route = external_route(&os_args(&[
            "zed",
            "--home",
            "/tmp/zed-home",
            "--git-submodules=false",
            "gitops",
            "validate",
        ]))
        .expect("external route");
        assert_eq!(
            route.environment,
            vec![
                (
                    OsString::from("ZED_PKG_HOME"),
                    OsString::from("/tmp/zed-home")
                ),
                (
                    OsString::from("ZED_PKG_GIT_SUBMODULES"),
                    OsString::from("false")
                ),
            ]
        );
    }

    #[test]
    fn root_options_after_plugin_are_lifted_until_double_dash() {
        let route = external_route(&os_args(&[
            "zed",
            "gitops",
            "validate",
            "--token",
            "fixture-value",
            "--offline",
            "--",
            "--home",
            "child-owned-value",
        ]))
        .expect("external route");
        assert_eq!(
            route.arguments,
            os_args(&["validate", "--offline", "--", "--home", "child-owned-value"])
        );
        assert_eq!(
            route.environment,
            vec![(
                OsString::from("ZED_PKG_TOKEN"),
                OsString::from("fixture-value")
            )]
        );
    }

    #[test]
    fn malformed_trailing_root_boolean_fails_closed() {
        assert!(
            external_route(&os_args(&[
                "zed",
                "gitops",
                "validate",
                "--git-submodules=maybe"
            ]))
            .is_none()
        );
    }

    #[test]
    fn help_spelling_routes_to_external_help() {
        let route = external_route(&os_args(&["zed", "help", "gitops", "validate"]))
            .expect("external help route");
        assert_eq!(route.name, "gitops");
        assert_eq!(route.arguments, os_args(&["validate", "--help"]));
    }

    #[test]
    fn builtins_and_unsafe_names_are_never_external() {
        assert!(is_builtin_name("install"));
        assert!(is_builtin_name("dev"));
        assert!(!is_builtin_name("gitops"));
        for invalid in ["../gitops", ".gitops", "git/ops", "-gitops", "gitops.exe"] {
            assert!(!valid_external_name(invalid), "accepted {invalid:?}");
        }
    }

    #[test]
    fn root_help_model_contains_the_known_external_contract() {
        let command = augment_root_command(crate::completion::built_in_root_command());
        let gitops = command.find_subcommand("gitops").expect("gitops command");
        assert!(gitops.find_subcommand("validate").is_some());
    }

    #[test]
    fn relative_path_entries_are_not_searched() {
        assert!(resolve_in_locations("demo", None, Some(OsStr::new("relative/bin"))).is_none());
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, exit_code: i32) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, format!("#!/bin/sh\nexit {exit_code}\n")).expect("write executable");
        let mut permissions = fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("set executable permissions");
    }

    #[cfg(unix)]
    #[test]
    fn sibling_precedes_absolute_path_and_exit_code_is_preserved() {
        let sibling = tempfile::tempdir().expect("sibling directory");
        let path_directory = tempfile::tempdir().expect("PATH directory");
        let sibling_executable = sibling.path().join("zed-demo");
        let path_executable = path_directory.path().join("zed-demo");
        write_executable(&sibling_executable, 23);
        write_executable(&path_executable, 41);
        let joined_path = env::join_paths([path_directory.path()]).expect("join PATH");

        let resolved =
            resolve_in_locations("demo", Some(sibling.path()), Some(joined_path.as_os_str()))
                .expect("resolved executable");
        assert_eq!(resolved, sibling_executable);

        let route = ExternalRoute {
            name: "demo".to_owned(),
            arguments: Vec::new(),
            environment: Vec::new(),
        };
        assert_eq!(run_external(&resolved, &route).expect("run external"), 23);
    }
}
