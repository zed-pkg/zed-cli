use std::collections::{BTreeSet, HashSet};

use clap::{Command, CommandFactory};
use toml::Value;
use zed_cli::cli::Cli;

#[derive(Debug)]
struct CliArg {
    path: Vec<String>,
    long: String,
    env: Option<String>,
}

#[derive(Debug)]
struct ContractFlag {
    path: Vec<String>,
    name: String,
    aliases: Vec<String>,
    env: Option<String>,
}

fn collect_clap(
    command: &Command,
    path: &mut Vec<String>,
    commands: &mut BTreeSet<String>,
    args: &mut Vec<CliArg>,
) {
    for arg in command.get_arguments() {
        let Some(long) = arg.get_long() else {
            continue;
        };
        if matches!(long, "help" | "version") {
            continue;
        }
        args.push(CliArg {
            path: path.clone(),
            long: long.to_owned(),
            env: arg
                .get_env()
                .map(|value| value.to_string_lossy().into_owned()),
        });
    }

    for child in command.get_subcommands() {
        path.push(child.get_name().to_owned());
        commands.insert(path.join(" "));
        collect_clap(child, path, commands, args);
        path.pop();
    }
}

fn collect_contract(
    node: &toml::map::Map<String, Value>,
    path: &mut Vec<String>,
    commands: &mut BTreeSet<String>,
    flags: &mut Vec<ContractFlag>,
) {
    if let Some(table) = node.get("flags").and_then(Value::as_table) {
        for (name, value) in table {
            let Some(flag) = value.as_table() else {
                continue;
            };
            let aliases = flag
                .get("aliases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let env = flag.get("env").and_then(Value::as_str).map(str::to_owned);
            flags.push(ContractFlag {
                path: path.clone(),
                name: name.clone(),
                aliases,
                env,
            });
        }
    }

    if let Some(table) = node.get("commands").and_then(Value::as_table) {
        for (name, value) in table {
            let Some(command) = value.as_table() else {
                continue;
            };
            path.push(name.clone());
            commands.insert(path.join(" "));
            collect_contract(command, path, commands, flags);
            path.pop();
        }
    }
}

#[test]
fn clap_and_cli_flags_contract_do_not_drift() {
    let source = std::fs::read_to_string(".cli-flags.toml")
        .expect("read repository .cli-flags.toml contract");
    let document = source
        .parse::<Value>()
        .expect("parse repository .cli-flags.toml contract");
    let root = document
        .as_table()
        .expect(".cli-flags.toml root must be a TOML table");

    let mut clap_commands = BTreeSet::new();
    let mut clap_args = Vec::new();
    collect_clap(
        &Cli::command(),
        &mut Vec::new(),
        &mut clap_commands,
        &mut clap_args,
    );

    let mut contract_commands = BTreeSet::new();
    let mut contract_flags = Vec::new();
    collect_contract(
        root,
        &mut Vec::new(),
        &mut contract_commands,
        &mut contract_flags,
    );

    let mut failures = Vec::new();

    for command in contract_commands.difference(&clap_commands) {
        failures.push(format!(
            "contract command `{command}` is not present in the typed Clap model"
        ));
    }
    for command in clap_commands.difference(&contract_commands) {
        failures.push(format!(
            "typed Clap command `{command}` is missing from .cli-flags.toml"
        ));
    }

    for arg in &clap_args {
        let candidates = contract_flags
            .iter()
            .filter(|flag| {
                flag.aliases.iter().any(|alias| alias == &arg.long)
                    && (flag.path.is_empty() || flag.path == arg.path)
            })
            .collect::<Vec<_>>();

        if candidates.is_empty() {
            failures.push(format!(
                "typed Clap option `--{}` at `{}` has no matching .cli-flags.toml alias",
                arg.long,
                if arg.path.is_empty() {
                    "<root>".to_owned()
                } else {
                    arg.path.join(" ")
                }
            ));
            continue;
        }

        if let Some(clap_env) = &arg.env
            && !candidates
                .iter()
                .any(|flag| flag.env.as_deref() == Some(clap_env.as_str()))
        {
            let contract_envs = candidates
                .iter()
                .filter_map(|flag| flag.env.as_deref())
                .collect::<BTreeSet<_>>();
            failures.push(format!(
                "typed Clap option `--{}` at `{}` binds `{clap_env}` but contract candidates bind {:?}",
                arg.long,
                if arg.path.is_empty() {
                    "<root>".to_owned()
                } else {
                    arg.path.join(" ")
                },
                contract_envs
            ));
        }
    }

    let clap_longs = clap_args
        .iter()
        .map(|arg| arg.long.as_str())
        .collect::<HashSet<_>>();
    for flag in &contract_flags {
        if flag.aliases.is_empty() {
            failures.push(format!(
                "contract flag `{}` at `{}` has no public aliases",
                flag.name,
                if flag.path.is_empty() {
                    "<root>".to_owned()
                } else {
                    flag.path.join(" ")
                }
            ));
            continue;
        }
        if !flag
            .aliases
            .iter()
            .any(|alias| clap_longs.contains(alias.as_str()))
        {
            failures.push(format!(
                "contract flag `{}` at `{}` has aliases {:?}, none of which exist in the typed Clap model",
                flag.name,
                if flag.path.is_empty() {
                    "<root>".to_owned()
                } else {
                    flag.path.join(" ")
                },
                flag.aliases
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "Clap/.cli-flags.toml parity drift:\n{}",
        failures.join("\n")
    );
}
