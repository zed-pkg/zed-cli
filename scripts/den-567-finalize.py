#!/usr/bin/env python3
"""Apply the final DEN-567 CI fixes on the feature branch.

The connected GitHub API exposes whole-file writes rather than patch writes.
This temporary helper performs exact, fail-closed edits in the branch runner,
then removes itself before the reviewable commit is pushed.
"""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def write(path: str, content: str) -> None:
    (ROOT / path).write_text(content, encoding="utf-8")


def replace_once(path: str, old: str, new: str) -> None:
    content = read(path)
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    write(path, content.replace(old, new, 1))


replace_once(
    "src/flags.rs",
    """    let argv = parser_argv(std::env::args());
    let explicit_envs = explicit_env_keys(&argv)?;
    let parsed = parse_embedded(&argv)?;
""",
    """    let argv = parser_argv(std::env::args());
    let parsed = parse_embedded(&argv)?;
""",
)

replace_once(
    "src/flags.rs",
    """    }

    for (key, value) in parsed.flags {
""",
    """    }

    let explicit_envs = explicit_env_keys(&argv, &parsed.subcommands)?;
    for (key, value) in parsed.flags {
""",
)

flags = read("src/flags.rs")
start = flags.index("fn explicit_env_keys(")
end = flags.index("\nfn parse_embedded(", start)
replacement = r'''fn explicit_env_keys(argv: &[String], subcommands: &[String]) -> Result<BTreeSet<String>> {
    let contract: toml::Value =
        toml::from_str(CONTRACT).context("parsing embedded flags2env contract")?;
    let aliases = active_flag_aliases(&contract, subcommands)?;

    let mut explicit = BTreeSet::new();
    for token in argv.iter().skip(1) {
        if token == "--" {
            break;
        }
        let Some(option) = token.strip_prefix('-') else {
            continue;
        };
        let option = option.trim_start_matches('-');
        if option.is_empty() {
            continue;
        }
        let name = option.split_once('=').map_or(option, |(name, _)| name);
        if let Some(env) = aliases.get(name) {
            explicit.insert(env.clone());
        }
    }
    Ok(explicit)
}

/// Build the option-to-environment map for the command path selected by
/// flags2env. Each deeper scope overlays its parent, matching flags2env's
/// command-first lookup. Reusing `--force` in `build` and `self-update`, for
/// example, is legal and must not be treated as a global alias collision.
fn active_flag_aliases(
    contract: &toml::Value,
    subcommands: &[String],
) -> Result<BTreeMap<String, String>> {
    let mut scope = contract
        .as_table()
        .context("embedded flags2env contract root must be a table")?;
    let mut aliases = BTreeMap::new();
    overlay_scope_flags(scope, &mut aliases)?;

    for command in subcommands {
        scope = find_command_scope(scope, command)?;
        overlay_scope_flags(scope, &mut aliases)?;
    }
    Ok(aliases)
}

fn find_command_scope<'a>(
    scope: &'a toml::value::Table,
    command: &str,
) -> Result<&'a toml::value::Table> {
    for keyword in ["commands", "command", "subcommands", "subcommand"] {
        let Some(commands) = scope.get(keyword).and_then(toml::Value::as_table) else {
            continue;
        };
        if let Some(selected) = commands.get(command).and_then(toml::Value::as_table) {
            return Ok(selected);
        }
        for candidate in commands.values().filter_map(toml::Value::as_table) {
            let matches_alias = candidate
                .get("aliases")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
                .any(|alias| alias == command);
            if matches_alias {
                return Ok(candidate);
            }
        }
    }
    bail!("flags2env selected command `{command}` is missing from the embedded contract")
}

fn overlay_scope_flags(
    scope: &toml::value::Table,
    aliases: &mut BTreeMap<String, String>,
) -> Result<()> {
    let Some(flags) = scope.get("flags").and_then(toml::Value::as_table) else {
        return Ok(());
    };

    for (canonical, flag) in flags {
        let env = flag
            .get("env")
            .and_then(toml::Value::as_str)
            .with_context(|| format!("flag `{canonical}` is missing its environment key"))?;
        let is_bool = flag
            .get("type")
            .and_then(toml::Value::as_str)
            .is_some_and(|kind| kind == "bool");
        let mut names = vec![canonical.replace('_', "-")];
        names.extend(
            flag.get("aliases")
                .and_then(toml::Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(toml::Value::as_str)
                .map(|alias| alias.trim_start_matches('-').to_string()),
        );
        if let Some(short) = flag.get("short").and_then(toml::Value::as_str) {
            names.push(short.trim_start_matches('-').to_string());
        }

        for name in names {
            aliases.insert(name.clone(), env.to_string());
            if is_bool && name.len() > 1 && !name.starts_with("no-") {
                aliases.insert(format!("no-{name}"), env.to_string());
            }
        }
    }
    Ok(())
}
'''
flags = flags[:start] + replacement + flags[end:]
write("src/flags.rs", flags)

flags = read("src/flags.rs")
start = flags.index("    #[test]\n    fn explicit_aliases_map_to_the_manifestless_environment_key()")
end = flags.index("    #[test]\n    fn precedence_is_explicit_cli_then_environment_then_default()", start)
tests = r'''    #[test]
    fn explicit_aliases_map_to_the_manifestless_environment_key() {
        for bypass in ["--allow-no-manifest", "--skip-manifest"] {
            let argv = vec![
                "zed".to_string(),
                "install".to_string(),
                "acme/http-kit@^1".to_string(),
                bypass.to_string(),
            ];
            let parsed = parse_embedded(&argv).unwrap();
            let keys = explicit_env_keys(&argv, &parsed.subcommands).unwrap();
            assert_eq!(
                keys,
                BTreeSet::from(["ZED_PKG_ALLOW_NO_MANIFEST".to_string()])
            );
        }
    }

    #[test]
    fn scoped_aliases_override_the_same_global_alias() {
        let argv = vec![
            "zed".to_string(),
            "self-update".to_string(),
            "--force".to_string(),
        ];
        let parsed = parse_embedded(&argv).unwrap();
        let keys = explicit_env_keys(&argv, &parsed.subcommands).unwrap();
        assert_eq!(keys, BTreeSet::from(["ZED_PKG_UPDATE_FORCE".to_string()]));

        let argv = vec![
            "zed".to_string(),
            "build".to_string(),
            "--force".to_string(),
        ];
        let parsed = parse_embedded(&argv).unwrap();
        let keys = explicit_env_keys(&argv, &parsed.subcommands).unwrap();
        assert_eq!(keys, BTreeSet::from(["ZED_PKG_FORCE".to_string()]));
    }

'''
flags = flags[:start] + tests + flags[end:]
write("src/flags.rs", flags)

replace_once(
    ".github/workflows/ci.yml",
    '''            _zed
            printf "%s\\n" "${COMPREPLY[@]}" | grep -Fx install
''',
    '''            _zed zed "" zed
            printf "%s\\n" "${COMPREPLY[@]}" | grep -Fx install
''',
)

print("DEN-567 final fixes applied")
