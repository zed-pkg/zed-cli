#!/usr/bin/env python3
"""Temporary idempotent integration for deterministic mise export."""

from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new, 1)


# Public module export.
path = Path("src/lib.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    "pub mod manifestless;\n",
    "pub mod manifestless;\npub mod mise_export;\n",
    "library module export",
)
path.write_text(text, encoding="utf-8")

# Typed clap surface and its direct parser regression.
path = Path("src/cli.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    "    /// Import or verify project-local mise configuration.\n    Mise,\n",
    "    /// Import, verify, or export project-local mise configuration.\n    Mise,\n",
    "environment manager help",
)

verify_anchor = """    /// Verify manager config/lock coverage and the normalized plan digest.
    Verify {
"""
export_variant = """    /// Export a schema-v2 EnvironmentPlan as deterministic mise TOML.
    Export {
        #[arg(value_enum)]
        manager: EnvironmentManagerArg,
        /// Project-local schema-v2 EnvironmentPlan (.toml or .json).
        #[arg(long, env = "ZED_PKG_ENV_PLAN")]
        plan: PathBuf,
        /// Project-local mise output path.
        #[arg(long, env = "ZED_PKG_ENV_OUTPUT", default_value = ".mise.toml")]
        output: PathBuf,
        /// Verify that the output already equals the deterministic projection.
        #[arg(long, env = "ZED_PKG_ENV_CHECK", conflicts_with = "write")]
        check: bool,
        /// Transactionally create/update a Zed-owned manager view.
        #[arg(long, env = "ZED_PKG_ENV_WRITE", conflicts_with = "check")]
        write: bool,
        /// Emit a machine-readable export result.
        #[arg(long, env = "ZED_PKG_ENV_JSON")]
        json: bool,
    },
    /// Verify manager config/lock coverage and the normalized plan digest.
    Verify {
"""
text = replace_once(text, verify_anchor, export_variant, "EnvCmd::Export")

old_test = """    #[test]
    fn completion_shells_are_typed_positionals() {
"""
new_test = """    #[test]
    fn environment_export_is_typed_and_rejects_ambiguous_write_modes() {
        let cli = Cli::try_parse_from([
            "zed",
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.toml",
            "--output",
            ".mise.toml",
            "--check",
            "--json",
        ])
        .unwrap();
        assert!(matches!(cli.cmd, Cmd::Env { .. }));

        assert!(Cli::try_parse_from([
            "zed",
            "env",
            "export",
            "mise",
            "--plan",
            "zed-env.toml",
            "--check",
            "--write",
        ])
        .is_err());
    }

    #[test]
    fn completion_shells_are_typed_positionals() {
"""
text = replace_once(text, old_test, new_test, "typed export CLI test")
path.write_text(text, encoding="utf-8")

# Binary dispatcher.
path = Path("src/main.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    "use zed_cli::managed_install;\n",
    "use zed_cli::managed_install;\nuse zed_cli::mise_export::{self, MiseExportMode};\n",
    "main export import",
)

verify_arm = """            EnvCmd::Verify {
                manager: _,
                config,
                lock,
                frozen,
                json,
            } => {
"""
export_arm = """            EnvCmd::Export {
                manager: _,
                plan,
                output,
                check,
                write,
                json,
            } => {
                let mode = if check {
                    MiseExportMode::Check
                } else if write {
                    MiseExportMode::Write
                } else {
                    MiseExportMode::Print
                };
                let exported = mise_export::export_mise(&cwd, &plan, &output, mode)?;
                mise_export::print_export(&exported, json)
            }
            EnvCmd::Verify {
                manager: _,
                config,
                lock,
                frozen,
                json,
            } => {
"""
text = replace_once(text, verify_arm, export_arm, "main export dispatch")
path.write_text(text, encoding="utf-8")

# flags-2-env registry and command documentation.
path = Path(".cli-flags.toml")
text = path.read_text(encoding="utf-8")
env_json = """[flags.env_json]
env = "ZED_PKG_ENV_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit environment interoperability output as JSON."
"""
env_flags = env_json + """
[flags.env_plan]
env = "ZED_PKG_ENV_PLAN"
aliases = ["plan"]
type = "string"
help = "Project-local schema-v2 environment plan path."

[flags.env_output]
env = "ZED_PKG_ENV_OUTPUT"
aliases = ["output"]
type = "string"
default = ".mise.toml"
help = "Project-local manager export path."

[flags.env_check]
env = "ZED_PKG_ENV_CHECK"
aliases = ["check"]
type = "bool"
default = "false"
help = "Verify the deterministic manager projection without writing."

[flags.env_write]
env = "ZED_PKG_ENV_WRITE"
aliases = ["write"]
type = "bool"
default = "false"
help = "Transactionally write a Zed-owned manager projection."
"""
text = replace_once(text, env_json, env_flags, "flags-2-env export flags")
text = replace_once(
    text,
    "[commands.env]\nhelp = \"Import and verify project-local developer environments.\"\n",
    "[commands.env]\nhelp = \"Import, verify, and export project-local developer environments.\"\n",
    "env command help",
)
verify_command = """[commands.env.commands.verify]
help = "Verify config/lock coverage and normalized environment identity."
"""
export_command = verify_command + """

[commands.env.commands.export]
help = "Project a schema-v2 EnvironmentPlan into deterministic manager configuration."
"""
text = replace_once(text, verify_command, export_command, "export command registry")
path.write_text(text, encoding="utf-8")

# Remove a no-longer-used import from the ordinary implementation.
path = Path("src/mise_export.rs")
text = path.read_text(encoding="utf-8")
text = text.replace(
    "use std::collections::{BTreeMap, BTreeSet};\n",
    "use std::collections::BTreeMap;\n",
    1,
)
path.write_text(text, encoding="utf-8")
