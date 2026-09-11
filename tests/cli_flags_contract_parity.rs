use std::collections::BTreeSet;

use clap::Command;
use toml::Value;

#[derive(Debug)]
struct CliArg {
    path: Vec<String>,
    long: String,
}

#[derive(Debug)]
struct ContractFlag {
    path: Vec<String>,
    name: String,
    spellings: Vec<String>,
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
        });
    }

    for child in command.get_subcommands() {
        if child.get_name() == "help" {
            continue;
        }
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
            let mut spellings = vec![name.replace('_', "-")];
            spellings.extend(
                flag.get("aliases")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
            spellings.sort();
            spellings.dedup();
            flags.push(ContractFlag {
                path: path.clone(),
                name: name.clone(),
                spellings,
            });
        }
    }

    if let Some(table) = node.get("commands").and_then(Value::as_table) {
        for (name, value) in table {
            let Some(command) = value.as_table() else {
                continue;
            };
            path.push(name.replace('_', "-"));
            commands.insert(path.join(" "));
            collect_contract(command, path, commands, flags);
            path.pop();
        }
    }
}

fn contract_scope_applies(contract_path: &[String], clap_path: &[String]) -> bool {
    contract_path.is_empty()
        || contract_path == clap_path
        || (contract_path.len() < clap_path.len()
            && clap_path.starts_with(contract_path))
}

#[test]
fn scope_matching_accepts_ancestors_but_not_siblings() {
    let release = vec!["release".to_owned()];
    let release_plan = vec!["release".to_owned(), "plan".to_owned()];
    let release_publish = vec!["release".to_owned(), "publish".to_owned()];
    let root: Vec<String> = Vec::new();

    assert!(contract_scope_applies(&root, &release_plan));
    assert!(contract_scope_applies(&release, &release));
    assert!(contract_scope_applies(&release, &release_plan));
    assert!(!contract_scope_applies(&release_plan, &release_publish));
    assert!(!contract_scope_applies(&release_publish, &release_plan));
}

#[test]
fn clap_command_paths_and_option_scopes_match_cli_flags_contract() {
    // `src/cli.rs::cli_flags_toml_is_in_sync_with_clap` already owns env-set
    // parity. This independent integration gate covers dimensions an env set
    // cannot detect: public command paths and the root/ancestor scope in which
    // a public long-option spelling is declared.
    //
    // Use the complete public root model rather than `Cli::command()` directly.
    // The production CLI intentionally composes several modular command
    // families (including OCI, overtake, graph/fetch/Nix/global, and external
    // built-ins) into help/completion through this exact authority.
    let source = std::fs::read_to_string(".cli-flags.toml")
        .expect("read repository .cli-flags.toml contract");
    let document: Value =
        toml::from_str(&source).expect("parse repository .cli-flags.toml contract");
    let root = document
        .as_table()
        .expect(".cli-flags.toml root must be a TOML table");

    let mut clap_commands = BTreeSet::new();
    let mut clap_args = Vec::new();
    collect_clap(
        &zed_cli::completion::root_command(),
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
            "contract command `{command}` is not present in the complete public Clap model"
        ));
    }
    for command in clap_commands.difference(&contract_commands) {
        failures.push(format!(
            "public Clap command `{command}` is missing from .cli-flags.toml"
        ));
    }

    for arg in &clap_args {
        let matched = contract_flags.iter().any(|flag| {
            flag.spellings.iter().any(|spelling| spelling == &arg.long)
                && contract_scope_applies(&flag.path, &arg.path)
        });
        if matched {
            continue;
        }

        let same_spelling_elsewhere = contract_flags
            .iter()
            .filter(|flag| flag.spellings.iter().any(|spelling| spelling == &arg.long))
            .map(|flag| {
                format!(
                    "{} ({})",
                    if flag.path.is_empty() {
                        "<root>".to_owned()
                    } else {
                        flag.path.join(" ")
                    },
                    flag.name
                )
            })
            .collect::<Vec<_>>();
        failures.push(format!(
            "public Clap option `--{}` at `{}` has no matching root/ancestor/same-scope .cli-flags.toml spelling; elsewhere={same_spelling_elsewhere:?}",
            arg.long,
            if arg.path.is_empty() {
                "<root>".to_owned()
            } else {
                arg.path.join(" ")
            }
        ));
    }

    assert!(
        failures.is_empty(),
        "Clap/.cli-flags.toml command/scope parity drift:\n{}",
        failures.join("\n")
    );
}
