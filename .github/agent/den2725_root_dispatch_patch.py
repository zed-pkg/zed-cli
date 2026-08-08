#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement target, found {count}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


Path("src/external_subcommands.rs").write_text(
    r'''//! Safe dispatch for separately installed `zed-*` command extensions.
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalRoute {
    name: String,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
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
pub fn augment_root_command(mut command: ClapCommand) -> ClapCommand {
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
            let mut arguments = args[index + 2..].to_vec();
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
        return Some(ExternalRoute {
            name: token.to_owned(),
            arguments: args[index + 1..].to_vec(),
            environment,
        });
    }

    None
}

fn root_value_option<'a>(token: &'a str) -> Option<(&'static str, Option<&'a str>)> {
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
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
        })
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

    if let Some(directory) = sibling.filter(|directory| directory.is_absolute()) {
        if let Some(executable) = executable_in(directory, &stem) {
            return Some(executable);
        }
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
        return status.signal().map(|signal| 128 + signal).unwrap_or(1);
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
            os_args(&[
                "validate",
                "--root",
                "workspace with spaces",
                "--offline"
            ])
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
                (OsString::from("ZED_PKG_HOME"), OsString::from("/tmp/zed-home")),
                (
                    OsString::from("ZED_PKG_GIT_SUBMODULES"),
                    OsString::from("false")
                ),
            ]
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
        assert!(
            resolve_in_locations("demo", None, Some(OsStr::new("relative/bin"))).is_none()
        );
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

        let resolved = resolve_in_locations(
            "demo",
            Some(sibling.path()),
            Some(joined_path.as_os_str()),
        )
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
''',
    encoding="utf-8",
)

Path("tests/external_gitops_dispatch.rs").write_text(
    r'''use std::process::{Command, Output};

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn root_help_advertises_gitops_validate() {
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .arg("--help")
        .output()
        .expect("run zed help");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(text.contains("gitops"), "{text}");
    assert!(text.contains("Validate GitOps composition"), "{text}");
}

#[test]
fn root_dispatches_to_the_sibling_gitops_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(["gitops", "validate", "--help"])
        .output()
        .expect("run zed gitops help");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(text.contains("Usage: zed-gitops validate"), "{text}");
    assert!(text.contains("--offline"), "{text}");
}

#[test]
fn root_help_alias_reaches_the_external_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(["help", "gitops"])
        .output()
        .expect("run zed help gitops");
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("Usage: zed-gitops"));
}
''',
    encoding="utf-8",
)

replace_once(
    "src/lib.rs",
    "pub mod environment;\npub mod fetch;",
    "pub mod environment;\npub mod external_subcommands;\npub mod fetch;",
)

replace_once(
    "src/main.rs",
    "    if let Some(result) = dev::dispatch(args) {\n",
    "    if let Some(result) = dev::dispatch(args.clone()) {\n",
)

replace_once(
    "src/main.rs",
    '''    if let Some(result) = dev::dispatch(args.clone()) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }

    if let Err(error) = zed_cli::flags::apply_cli_flags() {
''',
    '''    if let Some(result) = dev::dispatch(args.clone()) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }
    if let Some(result) = zed_cli::external_subcommands::dispatch(args) {
        match result {
            Ok(0) => return,
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::exit(1);
            }
        }
    }

    if let Err(error) = zed_cli::flags::apply_cli_flags() {
''',
)

replace_once(
    "src/completion.rs",
    "use crate::{dev, fetch, git_submodules, global, nix_bundle_write, nix_export_plan};",
    "use crate::{\n    dev, external_subcommands, fetch, git_submodules, global, nix_bundle_write,\n    nix_export_plan,\n};",
)

replace_once(
    "src/completion.rs",
    '''/// Build the complete public command tree shared by root help and completion
/// generation. Every modular command must compose here rather than maintaining
/// a second, partial root-help model.
pub fn root_command() -> clap::Command {
    global::augment_root_command(git_submodules::augment_root_command(
        nix_bundle_write::augment_root_command(nix_export_plan::augment_root_command(
            fetch::augment_root_command(dev::augment_root_command(cli_model::command())),
        )),
    ))
}
''',
    '''/// Build the complete built-in command tree without external extensions.
/// The external dispatcher uses this model to guarantee that a `zed-*`
/// executable can never shadow a built-in name or alias.
pub(crate) fn built_in_root_command() -> clap::Command {
    global::augment_root_command(git_submodules::augment_root_command(
        nix_bundle_write::augment_root_command(nix_export_plan::augment_root_command(
            fetch::augment_root_command(dev::augment_root_command(cli_model::command())),
        )),
    ))
}

/// Build the complete public command tree shared by root help and completion
/// generation. Every modular or external command must compose here rather than
/// maintaining a second, partial root-help model.
pub fn root_command() -> clap::Command {
    external_subcommands::augment_root_command(built_in_root_command())
}
''',
)

for occurrence in range(2):
    replace_once(
        "src/completion.rs",
        '''            "r2g",
        ] {''',
        '''            "r2g",
            "gitops",
            "validate",
        ] {''',
    )

for occurrence in range(2):
    replace_once(
        "src/completion.rs",
        '''            "--isolated-home",
        ] {''',
        '''            "--isolated-home",
            "--catalog",
            "--offline",
        ] {''',
    )

Path("docs/gitops-validator.md").write_text(
    '''# `zed gitops` external validator

`zed gitops` is the read-only GitOps validation lane tracked by DEN-2725. The
root `zed` binary now securely dispatches this command to the separately built
`zed-gitops` executable, while root help and shell completions expose the same
public command contract.

Install or build both binaries into the same bin directory:

```console
cargo install --path . --bins
zed gitops validate --root . --offline --strict
zed gitops validate --root . --offline --strict --format json
zed gitops validate --root . --offline --strict --format sarif
```

The standalone spelling remains supported for automation that deliberately
pins the validator executable:

```console
zed-gitops validate --root . --offline --strict
```

The dispatcher resolves `zed-gitops` beside the running `zed` executable first,
then searches only absolute `PATH` entries. It never invokes a shell, never
searches the current working directory implicitly, never permits an extension
to shadow a built-in command or alias, and preserves the child process exit
code. Root options placed before `gitops` are passed as their canonical
`ZED_PKG_*` environment variables rather than being exposed on the child
command line.

## Evidence checked

- catalog JSON is regular UTF-8 data beneath the selected repository root;
- unknown fields fail under `--strict`;
- `.gitmodules` provides exactly the declared inventory path and repository;
- the Git index contains that path as a mode-160000 gitlink;
- catalog inventory revision equals the indexed gitlink SHA;
- Argo source repository canonicalizes to the same upstream repository;
- Argo `targetRevision` is an exact lowercase 40-hex commit equal to the
  gitlink;
- the source is the direct app repository, not a path inside
  `ORESoftware/k8s-cluster`;
- application names and inventory paths are unique;
- `*-infra` repositories cannot be app records;
- AppProject and destination namespace cannot be `default`;
- `pilot-inert` records cannot enable automated sync, prune, or self-heal;
- the retained static Application is a regular parent-owned file.

The command does not read Kubernetes credentials, clone private repositories,
resolve remote branch tips, or apply manifests. Online validation is not
implemented yet, so invocations must pass `--offline`; omitting it fails
explicitly instead of misreporting a local-only run as online evidence. Policy
failures exit with code 2; tool/configuration failures exit with code 1.

## Ownership boundary

The root CLI owns extension discovery, built-in collision prevention, help,
completion, TTY inheritance, and exit-code propagation. `zed-gitops` owns the
current validation implementation. Follow-up work should expose the existing
`git_submodules` repository-identity and index primitives as a stable
`zed-pkg` library surface so the validator does not maintain parallel generic
Git parsing.

The deployment-specific schema and policy remain versioned in `k8s-cluster`;
Zed remains the validator UX rather than the deployment controller.
''',
    encoding="utf-8",
)
