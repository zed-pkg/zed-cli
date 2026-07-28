//! Apply the embedded flags-2-env contract before clap reads configuration.
//!
//! Clap remains the typed command parser. flags2env owns the portable
//! flag-to-environment contract and is statically linked so installed binaries
//! never depend on a source checkout or a shared library.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use anyhow::{Context, Result, bail};
use flags2env::{BundledFlags2Env, StructuredParse};

const CONTRACT: &str = include_str!("../.cli-flags.toml");

/// Audit and apply the embedded flags2env contract.
///
/// This runs once at process startup, before clap/configuration or any worker
/// threads. Unknown options and typed parse errors fail closed instead of
/// silently bypassing the contract. Precedence is explicit CLI flag, then an
/// existing process environment value, then the declarative contract default.
pub fn apply_cli_flags() -> Result<()> {
    let argv = parser_argv(std::env::args());
    let explicit_envs = explicit_env_keys(&argv)?;
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

    for (key, value) in parsed.flags {
        if should_apply_value(std::env::var_os(&key).is_some(), explicit_envs.contains(&key)) {
            // SAFETY: this function runs exactly once at process startup before
            // clap/configuration creates threads or reads the affected variables.
            unsafe { std::env::set_var(key, value) };
        }
    }
    Ok(())
}

fn should_apply_value(environment_exists: bool, explicitly_supplied: bool) -> bool {
    explicitly_supplied || !environment_exists
}

fn explicit_env_keys(argv: &[String]) -> Result<BTreeSet<String>> {
    let contract: toml::Value =
        toml::from_str(CONTRACT).context("parsing embedded flags2env contract")?;
    let mut aliases = BTreeMap::new();
    collect_flag_aliases(&contract, &mut aliases)?;

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

fn collect_flag_aliases(
    value: &toml::Value,
    aliases: &mut BTreeMap<String, String>,
) -> Result<()> {
    let Some(table) = value.as_table() else {
        return Ok(());
    };
    if let Some(flags) = table.get("flags").and_then(toml::Value::as_table) {
        for (canonical, flag) in flags {
            let env = flag
                .get("env")
                .and_then(toml::Value::as_str)
                .with_context(|| format!("flag `{canonical}` is missing its environment key"))?;
            let names = std::iter::once(canonical.replace('_', "-")).chain(
                flag.get("aliases")
                    .and_then(toml::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(toml::Value::as_str)
                    .map(|alias| alias.trim_start_matches('-').to_string()),
            );
            for name in names {
                if let Some(previous) = aliases.insert(name.clone(), env.to_string())
                    && previous != env
                {
                    bail!(
                        "flags2env alias `{name}` maps to both `{previous}` and `{env}`"
                    );
                }
            }
        }
    }
    for child in table.values() {
        collect_flag_aliases(child, aliases)?;
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
    fn explicit_aliases_map_to_the_manifestless_environment_key() {
        for bypass in ["--allow-no-manifest", "--skip-manifest"] {
            let keys = explicit_env_keys(&[
                "zed".to_string(),
                "install".to_string(),
                "acme/http-kit@^1".to_string(),
                bypass.to_string(),
            ])
            .unwrap();
            assert_eq!(
                keys,
                BTreeSet::from(["ZED_PKG_ALLOW_NO_MANIFEST".to_string()])
            );
        }
    }

    #[test]
    fn precedence_is_explicit_cli_then_environment_then_default() {
        assert!(should_apply_value(false, false));
        assert!(!should_apply_value(true, false));
        assert!(should_apply_value(true, true));
        assert!(should_apply_value(false, true));
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
    fn diagnostics_redact_inline_values() {
        assert_eq!(
            redact_option_value("--unknown=secret-value"),
            "--unknown=<redacted>"
        );
        assert_eq!(redact_option_value("plain diagnostic"), "plain diagnostic");
    }
}
