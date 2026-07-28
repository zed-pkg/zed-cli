#!/usr/bin/env python3
"""Apply the scoped boolean-environment normalization fix exactly once."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: str, old: str, new: str) -> None:
    target = ROOT / path
    content = target.read_text(encoding="utf-8")
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    target.write_text(content.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/flags.rs",
    '''    let explicit_envs = explicit_env_keys(&argv, &parsed.subcommands)?;
''',
    '''    normalize_active_boolean_environment(&parsed.subcommands)?;
    let explicit_envs = explicit_env_keys(&argv, &parsed.subcommands)?;
''',
)

replace_once(
    "src/flags.rs",
    '''fn should_apply_value(environment_exists: bool, explicitly_supplied: bool) -> bool {
    explicitly_supplied || !environment_exists
}

fn explicit_env_keys(argv: &[String], subcommands: &[String]) -> Result<BTreeSet<String>> {
''',
    '''fn should_apply_value(environment_exists: bool, explicitly_supplied: bool) -> bool {
    explicitly_supplied || !environment_exists
}

/// Clap's boolean environment parser accepts canonical `true`/`false`, while
/// shell and deployment configuration commonly use `1`, `0`, `yes`, `no`,
/// `on`, and `off`. Canonicalize only boolean keys declared in the active
/// flags2env scopes, before Clap reads them. Invalid values fail closed.
fn normalize_active_boolean_environment(subcommands: &[String]) -> Result<()> {
    let contract: toml::Value =
        toml::from_str(CONTRACT).context("parsing embedded flags2env contract")?;
    for env in active_boolean_env_keys(&contract, subcommands)? {
        let Some(raw) = std::env::var_os(&env) else {
            continue;
        };
        let raw = raw
            .to_str()
            .with_context(|| format!("boolean environment variable `{env}` is not valid UTF-8"))?;
        let normalized = normalize_boolean_value(raw).with_context(|| {
            format!(
                "boolean environment variable `{env}` must be one of \
                 true/false, 1/0, yes/no, or on/off"
            )
        })?;
        if raw != normalized {
            // SAFETY: apply_cli_flags runs once at process startup, before Clap
            // or any worker thread reads or mutates the process environment.
            unsafe { std::env::set_var(env, normalized) };
        }
    }
    Ok(())
}

fn normalize_boolean_value(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some("true"),
        "false" | "0" | "no" | "off" => Some("false"),
        _ => None,
    }
}

fn active_boolean_env_keys(
    contract: &toml::Value,
    subcommands: &[String],
) -> Result<BTreeSet<String>> {
    let mut scope = contract
        .as_table()
        .context("embedded flags2env contract root must be a table")?;
    let mut envs = BTreeSet::new();
    collect_scope_boolean_env_keys(scope, &mut envs)?;
    for command in subcommands {
        scope = find_command_scope(scope, command)?;
        collect_scope_boolean_env_keys(scope, &mut envs)?;
    }
    Ok(envs)
}

fn collect_scope_boolean_env_keys(
    scope: &toml::value::Table,
    envs: &mut BTreeSet<String>,
) -> Result<()> {
    let Some(flags) = scope.get("flags").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (canonical, flag) in flags {
        let is_bool = flag
            .get("type")
            .and_then(toml::Value::as_str)
            .is_some_and(|kind| kind == "bool");
        if !is_bool {
            continue;
        }
        let env = flag
            .get("env")
            .and_then(toml::Value::as_str)
            .with_context(|| format!("boolean flag `{canonical}` is missing its environment key"))?;
        envs.insert(env.to_string());
    }
    Ok(())
}

fn explicit_env_keys(argv: &[String], subcommands: &[String]) -> Result<BTreeSet<String>> {
''',
)

replace_once(
    "src/flags.rs",
    '''    #[test]
    fn precedence_is_explicit_cli_then_environment_then_default() {
''',
    '''    #[test]
    fn boolean_environment_values_are_canonicalized_and_invalid_values_fail() {
        for (input, expected) in [
            ("true", "true"),
            (" TRUE ", "true"),
            ("1", "true"),
            ("yes", "true"),
            ("ON", "true"),
            ("false", "false"),
            (" FALSE ", "false"),
            ("0", "false"),
            ("no", "false"),
            ("OFF", "false"),
        ] {
            assert_eq!(normalize_boolean_value(input), Some(expected));
        }
        for invalid in ["", "2", "maybe", "enabled"] {
            assert_eq!(normalize_boolean_value(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn install_scope_declares_the_manifestless_boolean_environment() {
        let contract: toml::Value = toml::from_str(CONTRACT).unwrap();
        let envs = active_boolean_env_keys(&contract, &["install".to_string()]).unwrap();
        assert!(envs.contains("ZED_PKG_ALLOW_NO_MANIFEST"));
        assert!(envs.contains("ZED_PKG_FROZEN"));
        assert!(!envs.contains("ZED_PKG_UPDATE_FORCE"));
    }

    #[test]
    fn precedence_is_explicit_cli_then_environment_then_default() {
''',
)

print("DEN-567 boolean environment normalization applied")
