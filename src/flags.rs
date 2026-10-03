//! Apply the embedded flags-2-env contract before clap reads configuration.
//!
//! Clap remains the typed command parser. flags2env owns the portable
//! flag-to-environment contract and is statically linked so installed binaries
//! never depend on a source checkout or a shared library.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;

use anyhow::{Context, Result, bail};
use flags2env::{BundledFlags2Env, StructuredParse};

use crate::env_map::{EnvMap, process_env_map};

const CONTRACT: &str = include_str!("../.cli-flags.toml");

/// Validate inherited global booleans before any modular route can short-circuit.
///
/// Root help is rendered by the modular `develop` router, before the legacy
/// command parser runs. This preflight keeps malformed deployment environment
/// values fail-closed even for `zed --help`, while preserving explicit CLI
/// precedence for valid inherited values. Clap still rejects malformed
/// inherited values instead of allowing argv to mask bad deployment state.
/// The full flags2env audit and parse still run for
/// established commands in [`apply_cli_flags`].
pub fn normalize_global_boolean_environment(args: &[OsString]) -> Result<()> {
    let argv = args
        .iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .context("flags2env requires UTF-8 command-line arguments")
        })
        .collect::<Result<Vec<_>>>()?;
    let explicit_envs = explicit_env_keys(&argv, &[])?;
    validate_active_boolean_environment(&process_env_map(), &[], &explicit_envs)?;
    Ok(())
}

/// Audit and apply the embedded flags2env contract.
///
/// This runs once at process startup, before clap/configuration or any worker
/// threads. Unknown options and typed parse errors fail closed instead of
/// silently bypassing the contract. Precedence is explicit CLI flag, then an
/// existing process environment value, then the declarative contract default.
/// Boolean spellings are canonicalized in the returned map; clap still reads
/// argv so explicit CLI flags remain visible to the typed parser.
pub fn apply_cli_flags() -> Result<EnvMap> {
    let argv = parser_argv(std::env::args());
    let parsed = parse_embedded(&argv)?;

    if !parsed.unknown_options.is_empty() {
        let options = parsed
            .unknown_options
            .iter()
            .map(|option| redact_option_value(option))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("flags2env rejected unknown option(s): {options}");
    }
    if !parsed.errors.is_empty() {
        bail!(
            "flags2env rejected invalid command-line value(s): {}",
            parsed
                .errors
                .iter()
                .map(|error| redact_option_value(error))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    let explicit_envs = explicit_env_keys(&argv, &parsed.subcommands)?;
    let mut env = process_env_map();
    normalize_boolean_values_in_map(&mut env, &parsed.subcommands, &explicit_envs)?;
    for (key, value) in parsed.flags {
        if should_apply_value(env.contains_key(&key), explicit_envs.contains(&key)) {
            env.insert(key, value);
        }
    }
    Ok(env)
}

fn should_apply_value(environment_exists: bool, explicitly_supplied: bool) -> bool {
    explicitly_supplied || !environment_exists
}

/// Clap's boolean environment parser accepts canonical `true`/`false`, while
/// shell and deployment configuration commonly use `1`, `0`, `yes`, `no`,
/// `on`, and `off`. Canonicalize only boolean keys declared in the active
/// flags2env scopes. Invalid values fail closed. This never writes `std::env`.
fn normalize_boolean_values_in_map(
    env: &mut EnvMap,
    subcommands: &[String],
    explicit_envs: &BTreeSet<String>,
) -> Result<()> {
    let contract: toml::Value =
        toml::from_str(CONTRACT).context("parsing embedded flags2env contract")?;
    for key in active_boolean_env_keys(&contract, subcommands)? {
        let Some(raw) = env.get(&key).cloned() else {
            continue;
        };
        let Some(normalized) =
            normalized_environment_boolean(&key, OsStr::new(&raw), explicit_envs.contains(&key))?
        else {
            continue;
        };
        if raw != normalized {
            env.insert(key, normalized.to_string());
        }
    }
    Ok(())
}

fn validate_active_boolean_environment(
    env: &EnvMap,
    subcommands: &[String],
    explicit_envs: &BTreeSet<String>,
) -> Result<()> {
    let mut snapshot = env.clone();
    normalize_boolean_values_in_map(&mut snapshot, subcommands, explicit_envs)?;
    Ok(())
}

fn normalized_environment_boolean(
    env: &str,
    raw: &OsStr,
    explicitly_supplied: bool,
) -> Result<Option<&'static str>> {
    if explicitly_supplied {
        return Ok(None);
    }
    let raw = raw
        .to_str()
        .with_context(|| format!("boolean environment variable `{env}` is not valid UTF-8"))?;
    normalize_boolean_value(raw).map(Some).with_context(|| {
        format!(
            "boolean environment variable `{env}` must be one of true/false, 1/0, yes/no, or on/off"
        )
    })
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
            .with_context(|| {
                format!("boolean flag `{canonical}` is missing its environment key")
            })?;
        envs.insert(env.to_string());
    }
    Ok(())
}

fn explicit_env_keys(argv: &[String], subcommands: &[String]) -> Result<BTreeSet<String>> {
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

fn parse_embedded(argv: &[String]) -> Result<StructuredParse> {
    let contract_dir = tempfile::tempdir().context("creating embedded flags2env contract dir")?;
    let contract_path = contract_dir.path().join(".cli-flags.toml");
    fs::write(&contract_path, CONTRACT).context("writing embedded flags2env contract")?;
    let contract_path = contract_path
        .to_str()
        .context("embedded flags2env contract path is not valid UTF-8")?;

    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract_path))
        .map_err(|error| anyhow::anyhow!("flags2env contract audit failed: {error}"))?;
    parser
        .parse_structured(argv, Some(contract_path))
        .map_err(|error| anyhow::anyhow!("flags2env parse failed: {error}"))
}

/// Clap owns its built-in help/version behavior. Removing only these exact
/// tokens from the flags2env copy avoids treating them as undeclared options;
/// the original process argv is unchanged and clap still sees them.
fn parser_argv(argv: impl IntoIterator<Item = String>) -> Vec<String> {
    argv.into_iter()
        .filter(|token| !matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
        .collect()
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

    #[test]
    fn embedded_contract_audits_and_maps_both_manifestless_spellings() {
        for bypass in ["--allow-no-manifest", "--skip-manifest"] {
            let argv = vec![
                "zed".to_string(),
                "install".to_string(),
                bypass.to_string(),
                "--install-mode=copy".to_string(),
                "zed-pkg-test/portable-greeter@^1".to_string(),
            ];
            let parsed = parse_embedded(&argv).expect("embedded contract must parse");
            assert!(parsed.unknown_options.is_empty(), "{bypass}");
            assert!(parsed.errors.is_empty(), "{bypass}");
            assert_eq!(
                parsed
                    .flags
                    .get("ZED_PKG_ALLOW_NO_MANIFEST")
                    .map(String::as_str),
                Some("true"),
                "{bypass}"
            );
            assert_eq!(
                parsed.flags.get("ZED_PKG_INSTALL_MODE").map(String::as_str),
                Some("copy")
            );
        }
    }

    #[test]
    fn embedded_contract_accepts_project_cli_runtime_flags() {
        let argv = vec![
            "zed".to_string(),
            "install".to_string(),
            "--cli".to_string(),
            "nodejs".to_string(),
            "--cli".to_string(),
            "python3".to_string(),
            "--cli-target=x86_64-unknown-linux-gnu".to_string(),
            "--cli-install-mode=copy".to_string(),
        ];
        let parsed = parse_embedded(&argv).expect("embedded contract must parse CLI tools");
        assert!(parsed.unknown_options.is_empty());
        assert!(parsed.errors.is_empty());
        assert_eq!(
            parsed.flags.get("ZED_PKG_CLI_TARGET").map(String::as_str),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            parsed
                .flags
                .get("ZED_PKG_CLI_INSTALL_MODE")
                .map(String::as_str),
            Some("copy")
        );
    }

    #[test]
    fn embedded_contract_accepts_global_mirror_controls() {
        for (argv, expected_env) in [
            (vec!["zed", "--no-mirrors", "install"], "ZED_PKG_NO_MIRRORS"),
            (
                vec!["zed", "install", "--trust-mirror-metadata"],
                "ZED_PKG_TRUST_MIRROR_METADATA",
            ),
        ] {
            let argv = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
            let parsed = parse_embedded(&argv).expect("mirror controls must parse");
            assert!(parsed.unknown_options.is_empty(), "{argv:?}");
            assert!(parsed.errors.is_empty(), "{argv:?}");
            assert_eq!(
                parsed.flags.get(expected_env).map(String::as_str),
                Some("true"),
                "{argv:?}"
            );
            assert!(
                explicit_env_keys(&argv, &parsed.subcommands)
                    .expect("mirror controls must resolve their environment key")
                    .contains(expected_env),
                "{argv:?}"
            );
        }
    }

    #[test]
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

    #[test]
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
    fn lifecycle_consent_flags_map_independently() {
        let argv = vec![
            "zed".to_string(),
            "install".to_string(),
            "--allow-native-deps".to_string(),
            "--allow-install-hooks".to_string(),
            "--allow-build".to_string(),
        ];
        let parsed = parse_embedded(&argv).unwrap();
        for env in [
            "ZED_PKG_ALLOW_NATIVE_DEPS",
            "ZED_PKG_ALLOW_INSTALL_HOOKS",
            "ZED_PKG_ALLOW_BUILD",
        ] {
            assert_eq!(parsed.flags.get(env).map(String::as_str), Some("true"));
        }
        assert_eq!(
            explicit_env_keys(&argv, &parsed.subcommands).unwrap(),
            BTreeSet::from([
                "ZED_PKG_ALLOW_BUILD".to_string(),
                "ZED_PKG_ALLOW_INSTALL_HOOKS".to_string(),
                "ZED_PKG_ALLOW_NATIVE_DEPS".to_string(),
            ])
        );
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
        assert!(should_apply_value(false, false));
        assert!(!should_apply_value(true, false));
        assert!(should_apply_value(true, true));
        assert!(should_apply_value(false, true));
    }

    #[test]
    fn explicit_cli_boolean_shadows_even_an_invalid_environment_value() {
        let invalid = OsStr::new("maybe");
        assert_eq!(
            normalized_environment_boolean("ZED_PKG_ALLOW_NO_MANIFEST", invalid, true).unwrap(),
            None
        );
        assert!(
            normalized_environment_boolean("ZED_PKG_ALLOW_NO_MANIFEST", invalid, false)
                .unwrap_err()
                .to_string()
                .contains("true/false")
        );
    }

    #[test]
    fn embedded_contract_reports_unknown_options_but_allows_run_passthrough() {
        let unknown = vec!["zed".to_string(), "--not-a-zed-option".to_string()];
        let parsed = parse_embedded(&unknown).expect("structured parse must complete");
        assert_eq!(parsed.unknown_options, ["--not-a-zed-option"]);

        let passthrough = vec![
            "zed".to_string(),
            "run".to_string(),
            "tool".to_string(),
            "--tool-owned-option".to_string(),
        ];
        let parsed = parse_embedded(&passthrough).expect("run passthrough must parse");
        assert!(parsed.unknown_options.is_empty());
    }

    #[test]
    fn help_and_version_tokens_are_left_for_clap() {
        let filtered = parser_argv([
            "zed".to_string(),
            "install".to_string(),
            "--help".to_string(),
            "-V".to_string(),
        ]);
        assert_eq!(filtered, ["zed", "install"]);
    }

    #[test]
    fn long_inline_values_are_not_truncated() {
        let token = format!("header.{}.signature", "a".repeat(240));
        let argv = vec![
            "zed".to_string(),
            format!("--token={token}"),
            "find".to_string(),
            "http".to_string(),
        ];
        let parsed = parse_embedded(&argv).expect("long inline values must parse");
        assert!(parsed.unknown_options.is_empty());
        assert!(parsed.errors.is_empty());
        assert_eq!(
            parsed.flags.get("ZED_PKG_TOKEN").map(String::as_str),
            Some(token.as_str())
        );
    }

    #[test]
    fn diagnostics_redact_inline_values() {
        assert_eq!(
            redact_option_value("--unknown=secret-value"),
            "--unknown=<redacted>"
        );
        assert_eq!(redact_option_value("plain diagnostic"), "plain diagnostic");
    }

    #[test]
    fn source_does_not_write_process_environment() {
        const SRC: &str = include_str!("flags.rs");
        let production = SRC.split("#[cfg(test)]").next().unwrap_or(SRC);
        assert!(!production.contains("set_var"));
    }
}
