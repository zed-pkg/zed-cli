use std::{collections::BTreeSet, fs, path::PathBuf};

use clap::Command;
use toml::Value;

#[derive(Debug)]
struct CliArg {
    path: Vec<String>,
    long: String,
    env: Option<String>,
}

#[derive(Debug)]
struct ContractFlag {
    contract: String,
    path: Vec<String>,
    name: String,
    spellings: Vec<String>,
    env: Option<String>,
}

fn contract_paths() -> Vec<PathBuf> {
    let mut paths = fs::read_dir(".")
        .expect("read repository root")
        .map(|entry| entry.expect("read repository-root entry").path())
        .filter(|path| {
            path.is_file()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name == ".cli-flags.toml"
                            || (name.starts_with('.') && name.ends_with("-cli-flags.toml"))
                    })
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn root_command_contract(root: &toml::map::Map<String, Value>) -> bool {
    root.get("parse")
        .and_then(Value::as_table)
        .and_then(|parse| parse.get("command_env"))
        .and_then(Value::as_str)
        == Some("ZED_PKG_COMMAND")
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
                .and_then(|env| env.to_str())
                .map(str::to_owned),
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
    contract: &str,
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
                contract: contract.to_owned(),
                path: path.clone(),
                name: name.clone(),
                spellings,
                env: flag.get("env").and_then(Value::as_str).map(str::to_owned),
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
            collect_contract(contract, command, path, commands, flags);
            path.pop();
        }
    }
}

fn scope_is_ancestor_or_same(owner: &[String], public_path: &[String]) -> bool {
    !owner.is_empty() && owner.len() <= public_path.len() && public_path.starts_with(owner)
}

fn flag_scope_applies(contract_path: &[String], clap_path: &[String]) -> bool {
    contract_path.is_empty()
        || contract_path == clap_path
        || (contract_path.len() < clap_path.len() && clap_path.starts_with(contract_path))
}

fn root_name(path: &[String]) -> Option<&str> {
    path.first().map(String::as_str)
}

fn command_roots(command: &Command) -> BTreeSet<String> {
    command
        .get_subcommands()
        .filter(|child| child.get_name() != "help")
        .map(|child| child.get_name().to_owned())
        .collect()
}

fn non_flags2env_public_roots() -> BTreeSet<String> {
    // The complete help/completion model is the typed runtime model plus any
    // separately installed zed-* extension models. The set difference derives
    // external namespaces from production composition rather than a hand-kept
    // exclusion list.
    let public = zed_cli::completion::root_command();
    let typed = zed_cli::cli_model::command();
    let public_roots = command_roots(&public);
    let typed_roots = command_roots(&typed);
    let external = public_roots
        .difference(&typed_roots)
        .cloned()
        .collect::<BTreeSet<_>>();

    // Keep this fail closed: a new external help model needs its own executable
    // boundary test before this ownership gate silently accepts it.
    assert_eq!(external, BTreeSet::from(["gitops".to_owned()]));

    // inspect is the sole production-declared read-only early dispatcher. It is
    // attached to cli_model for help/parser parity but prepare_environment exits
    // through inspect::dispatch before flags::apply_cli_flags. Derive its name
    // from the command itself rather than duplicating a string in the scanner.
    let inspect = zed_cli::inspect::command().get_name().to_owned();
    assert_eq!(inspect, "inspect");
    assert!(typed_roots.contains(&inspect));

    let mut boundaries = external;
    boundaries.insert(inspect);
    boundaries
}

#[test]
fn scope_matching_accepts_ancestors_but_not_siblings() {
    let release = vec!["release".to_owned()];
    let release_plan = vec!["release".to_owned(), "plan".to_owned()];
    let release_publish = vec!["release".to_owned(), "publish".to_owned()];
    let root: Vec<String> = Vec::new();

    assert!(flag_scope_applies(&root, &release_plan));
    assert!(flag_scope_applies(&release, &release));
    assert!(flag_scope_applies(&release, &release_plan));
    assert!(!flag_scope_applies(&release_plan, &release_publish));
    assert!(!flag_scope_applies(&release_publish, &release_plan));

    assert!(scope_is_ancestor_or_same(&release, &release_plan));
    assert!(!scope_is_ancestor_or_same(&release_plan, &release));
    assert!(!scope_is_ancestor_or_same(&release_plan, &release_publish));
}

#[test]
fn public_cli_that_reaches_flags2env_is_owned_by_repository_contracts() {
    let mut clap_commands = BTreeSet::new();
    let mut clap_args = Vec::new();
    collect_clap(
        &zed_cli::completion::root_command(),
        &mut Vec::new(),
        &mut clap_commands,
        &mut clap_args,
    );

    let non_flags2env = non_flags2env_public_roots();
    assert_eq!(
        non_flags2env,
        BTreeSet::from(["gitops".to_owned(), "inspect".to_owned()])
    );

    let paths = contract_paths();
    assert!(
        paths.len() >= 6,
        "expected canonical plus modular flags2env contracts"
    );

    let mut contract_commands = BTreeSet::new();
    let mut contract_flags = Vec::new();
    let mut root_contract_count = 0usize;
    let mut helper_contract_count = 0usize;
    for path in paths {
        let display = path.display().to_string();
        let source =
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {display}: {error}"));
        let document: Value =
            toml::from_str(&source).unwrap_or_else(|error| panic!("parse {display}: {error}"));
        let root = document
            .as_table()
            .unwrap_or_else(|| panic!("{display} root must be a TOML table"));
        if !root_command_contract(root) {
            helper_contract_count += 1;
            continue;
        }
        root_contract_count += 1;
        collect_contract(
            &display,
            root,
            &mut Vec::new(),
            &mut contract_commands,
            &mut contract_flags,
        );
    }
    assert!(
        root_contract_count >= 4,
        "expected root plus modular ZED_PKG_COMMAND contracts"
    );
    assert!(
        helper_contract_count >= 2,
        "expected independent helper-binary flags2env namespaces"
    );

    // A separately dispatched namespace must never be accidentally claimed by
    // the root ZED_PKG_COMMAND contracts. That would collapse the boundary the
    // runtime and black-box helper tests deliberately maintain.
    for boundary in &non_flags2env {
        assert!(
            !contract_commands
                .iter()
                .any(|command| command == boundary || command.starts_with(&format!("{boundary} "))),
            "non-flags2env namespace `{boundary}` was accidentally claimed by a root contract"
        );
    }

    let mut failures = Vec::new();

    for command in &contract_commands {
        if !clap_commands.contains(command) {
            failures.push(format!(
                "root flags2env contract command `{command}` is not present in the complete public Clap model"
            ));
        }
    }

    for command in &clap_commands {
        let public_path = command.split(' ').map(str::to_owned).collect::<Vec<_>>();
        if root_name(&public_path).is_some_and(|root| non_flags2env.contains(root)) {
            continue;
        }
        let owned = contract_commands.iter().any(|owner| {
            let owner_path = owner.split(' ').map(str::to_owned).collect::<Vec<_>>();
            scope_is_ancestor_or_same(&owner_path, &public_path)
        });
        if !owned {
            failures.push(format!(
                "public Clap command `{command}` reaches flags2env but has no repository-owned ZED_PKG_COMMAND contract"
            ));
        }
    }

    for arg in &clap_args {
        if root_name(&arg.path).is_some_and(|root| non_flags2env.contains(root)) {
            continue;
        }

        let candidates = contract_flags
            .iter()
            .filter(|flag| {
                flag.spellings.iter().any(|spelling| spelling == &arg.long)
                    && flag_scope_applies(&flag.path, &arg.path)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            let same_spelling_elsewhere = contract_flags
                .iter()
                .filter(|flag| flag.spellings.iter().any(|spelling| spelling == &arg.long))
                .map(|flag| {
                    format!(
                        "{}:{} ({})",
                        flag.contract,
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
                "public Clap option `--{}` at `{}` has no matching root/ancestor/same-scope ZED_PKG_COMMAND flags2env spelling; elsewhere={same_spelling_elsewhere:?}",
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
            failures.push(format!(
                "public Clap option `--{}` at `{}` binds env `{clap_env}` but matching flags2env owners bind {:?}",
                arg.long,
                if arg.path.is_empty() {
                    "<root>".to_owned()
                } else {
                    arg.path.join(" ")
                },
                candidates
                    .iter()
                    .map(|flag| flag.env.as_deref().unwrap_or("<none>"))
                    .collect::<Vec<_>>()
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "public CLI/flags2env ownership drift:\n{}",
        failures.join("\n")
    );
}
