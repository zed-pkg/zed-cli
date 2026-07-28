//! Apply the embedded flags-2-env contract before clap reads configuration.
//!
//! Clap remains the typed command parser. flags2env owns the portable
//! flag-to-environment contract and is statically linked so installed binaries
//! never depend on a source checkout or a shared library.

use std::fs;

use anyhow::{Context, Result, bail};
use flags2env::{BundledFlags2Env, StructuredParse};

const CONTRACT: &str = include_str!("../.cli-flags.toml");

/// Audit and apply the embedded flags2env contract.
///
/// This runs once at process startup, before clap/configuration or any worker
/// threads. Unknown options and typed parse errors fail closed instead of
/// silently bypassing the contract.
pub fn apply_cli_flags() -> Result<()> {
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

    for (key, value) in parsed.flags {
        // SAFETY: this function runs exactly once at process startup before
        // clap/configuration creates threads or reads the affected variables.
        unsafe { std::env::set_var(key, value) };
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
    fn embedded_contract_audits_and_maps_explicit_flags() {
        let argv = vec![
            "zed".to_string(),
            "install".to_string(),
            "--allow-no-manifest".to_string(),
            "--install-mode=copy".to_string(),
            "zed-pkg-test/portable-greeter@^1".to_string(),
        ];
        let parsed = parse_embedded(&argv).expect("embedded contract must parse");
        assert!(parsed.unknown_options.is_empty());
        assert!(parsed.errors.is_empty());
        assert_eq!(
            parsed
                .flags
                .get("ZED_PKG_ALLOW_NO_MANIFEST")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            parsed.flags.get("ZED_PKG_INSTALL_MODE").map(String::as_str),
            Some("copy")
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
    fn diagnostics_redact_inline_values() {
        assert_eq!(
            redact_option_value("--unknown=secret-value"),
            "--unknown=<redacted>"
        );
        assert_eq!(redact_option_value("plain diagnostic"), "plain diagnostic");
    }
}
