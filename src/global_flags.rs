//! flags2env admission for the modular `zed global` command family.
//!
//! The global package implementation keeps Clap as its typed parser. This
//! module supplies an independent fail-closed option/environment contract that
//! runs before profile or PATH mutation begins.

use std::env;
use std::ffi::OsString;
use std::fs;

use anyhow::{Context, Result, bail};
use flags2env::BundledFlags2Env;

const GLOBAL_CONTRACT: &str = include_str!("../.global-cli-flags.toml");

pub(crate) fn validate(argv: &[OsString]) -> Result<()> {
    let string_args = utf8_args(argv)?;
    normalize_boolean_environment()?;
    let parser_argv = string_args
        .into_iter()
        .filter(|token| !matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
        .collect::<Vec<_>>();
    let parsed = parse_embedded(&parser_argv)?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "flags2env rejected unknown zed global option(s): {}",
            parsed
                .unknown_options
                .iter()
                .map(|value| redact_option_value(value))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !parsed.errors.is_empty() {
        bail!(
            "flags2env rejected invalid zed global value(s): {}",
            parsed
                .errors
                .iter()
                .map(|value| redact_option_value(value))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(())
}

fn utf8_args(argv: &[OsString]) -> Result<Vec<String>> {
    argv.iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .context("flags-2-env requires UTF-8 command-line arguments")
        })
        .collect()
}

fn parse_embedded(argv: &[String]) -> Result<flags2env::StructuredParse> {
    let contract_dir = tempfile::tempdir().context("creating zed global flags2env directory")?;
    let contract_path = contract_dir.path().join(".cli-flags.toml");
    fs::write(&contract_path, GLOBAL_CONTRACT).context("writing embedded zed global contract")?;
    let contract_path = contract_path
        .to_str()
        .context("embedded zed global contract path is not valid UTF-8")?;

    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed global flags2env audit failed: {error}"))?;
    parser
        .parse_structured(argv, Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed global flags2env parse failed: {error}"))
}

fn normalize_boolean_environment() -> Result<()> {
    for key in [
        "ZED_PKG_INTERACTIVE",
        "ZED_PKG_GIT_SUBMODULES",
        "ZED_PKG_NO_MIRRORS",
        "ZED_PKG_TRUST_MIRROR_METADATA",
        "ZED_PKG_SOURCE_FALLBACK",
        "ZED_PKG_FROZEN",
        "ZED_PKG_ALLOW_BUILD",
    ] {
        let Some(raw) = env::var_os(key) else {
            continue;
        };
        let raw = raw
            .to_str()
            .with_context(|| format!("boolean environment variable `{key}` is not UTF-8"))?;
        let normalized = match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => "true",
            "false" | "0" | "no" | "off" => "false",
            _ => bail!(
                "boolean environment variable `{key}` must be true/false, 1/0, yes/no, or on/off"
            ),
        };
        if raw != normalized {
            // SAFETY: modular global dispatch runs at process startup before
            // worker threads, matching the existing graph/fetch boundary.
            unsafe { env::set_var(key, normalized) };
        }
    }
    Ok(())
}

fn redact_option_value(value: &str) -> String {
    match value.split_once('=') {
        Some((option, _)) if option.starts_with('-') => format!("{option}=<redacted>"),
        _ => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml::Value;

    fn argv(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn collect_public_envs(value: &Value, envs: &mut Vec<String>) {
        let Some(table) = value.as_table() else {
            return;
        };
        if let Some(flags) = table.get("flags").and_then(Value::as_table) {
            for flag in flags.values().filter_map(Value::as_table) {
                if let Some(env) = flag.get("env").and_then(Value::as_str) {
                    envs.push(env.to_owned());
                }
            }
        }
        for (name, child) in table {
            if name != "flags" {
                collect_public_envs(child, envs);
            }
        }
    }

    #[test]
    fn embedded_global_contract_accepts_public_install_surface() {
        let parsed = parse_embedded(&argv(&[
            "zed",
            "--global-bin-dir",
            "/tmp/zed-bin",
            "global",
            "install",
            "acme/tool@1.2.3",
            "--frozen",
            "--install-mode",
            "copy",
            "--allow-build",
            "--target",
            "rust",
        ]))
        .expect("global contract should parse canonical install argv");
        assert!(
            parsed.unknown_options.is_empty(),
            "{:?}",
            parsed.unknown_options
        );
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
    }

    #[test]
    fn embedded_global_contract_is_fail_closed() {
        let parsed = parse_embedded(&argv(&[
            "zed",
            "global",
            "install",
            "acme/tool",
            "--not-a-global-option",
        ]))
        .expect("unknown options should be structured evidence");
        assert!(!parsed.unknown_options.is_empty());
    }

    #[test]
    fn global_registry_token_is_environment_only() {
        let document: Value = toml::from_str(GLOBAL_CONTRACT).expect("parse global contract");
        let ignored = document
            .get("env")
            .and_then(Value::as_table)
            .and_then(|env| env.get("ignore"))
            .and_then(Value::as_array)
            .expect("global contract must declare [env].ignore")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert!(ignored.contains(&"ZED_PKG_TOKEN"));

        let mut public_envs = Vec::new();
        collect_public_envs(&document, &mut public_envs);
        assert!(!public_envs.iter().any(|env| env == "ZED_PKG_TOKEN"));
    }
}
