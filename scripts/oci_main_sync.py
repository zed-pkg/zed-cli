#!/usr/bin/env python3
"""One-shot semantic merge of the certified OCI stack onto current main."""

from __future__ import annotations

import subprocess
from pathlib import Path


def run(*args: str, check: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        list(args),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    print(result.stdout, end="")
    if check and result.returncode != 0:
        raise SystemExit(f"command failed ({result.returncode}): {' '.join(args)}")
    return result


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    return text.replace(old, new)


def merge_main() -> None:
    run("git", "fetch", "--no-tags", "origin", "main")
    run("git", "merge", "--no-ff", "--no-commit", "origin/main", check=False)
    conflicts = run(
        "git", "diff", "--name-only", "--diff-filter=U"
    ).stdout.splitlines()
    allowed = {
        ".cli-flags.toml",
        "Cargo.lock",
        "Cargo.toml",
        "README.md",
        "src/cli.rs",
        "src/lib.rs",
        "src/main.rs",
    }
    for path in conflicts:
        if path not in allowed:
            raise SystemExit(f"unrecognized semantic merge conflict: {path}")
        run("git", "checkout", "--theirs", "--", path)
        run("git", "add", "--", path)
    unresolved = run(
        "git", "diff", "--name-only", "--diff-filter=U"
    ).stdout.strip()
    if unresolved:
        raise SystemExit(f"unresolved merge conflicts remain:\n{unresolved}")


def patch_cli() -> None:
    path = Path("src/cli.rs")
    text = path.read_text(encoding="utf-8")
    import_line = "use crate::cli_oci::OciCmd;\n"
    if import_line not in text:
        anchor = "use clap::{Args, Parser, Subcommand, ValueEnum};\n"
        text = replace_once(text, anchor, anchor + "\n" + import_line, "cli import")

    if "    Oci {\n" not in text:
        anchor = '''    /// Plan a coordinated Zed + native-registry release without credentials or uploads
    Release {
        #[command(subcommand)]
        cmd: ReleaseCmd,
    },
'''
        variant = '''    /// Plan, materialize, and distribute immutable Zed packages through OCI registries
    Oci {
        #[command(subcommand)]
        cmd: OciCmd,
    },
'''
        text = replace_once(text, anchor, anchor + variant, "OCI command")
    path.write_text(text, encoding="utf-8")


def patch_lib() -> None:
    path = Path("src/lib.rs")
    text = path.read_text(encoding="utf-8")
    if "pub mod cli_oci;\n" not in text:
        text = replace_once(
            text,
            "pub mod cli;\n",
            "pub mod cli;\npub mod cli_oci;\n",
            "lib cli module",
        )
    if "pub mod oci;\n" not in text:
        text = replace_once(
            text,
            "pub mod nix_export_plan;\n",
            "pub mod nix_export_plan;\npub mod oci;\npub mod oci_layout;\npub mod oci_push;\n",
            "lib OCI modules",
        )
    path.write_text(text, encoding="utf-8")


def patch_main() -> None:
    path = Path("src/main.rs")
    text = path.read_text(encoding="utf-8")
    if "use zed_cli::cli_oci::OciCmd;\n" not in text:
        text = replace_once(
            text,
            "use zed_cli::cli::EnvCmd;\n",
            "use zed_cli::cli::EnvCmd;\nuse zed_cli::cli_oci::OciCmd;\n",
            "main OCI command import",
        )
    if "use zed_cli::oci_push::{self, OciPushOptions};\n" not in text:
        text = replace_once(
            text,
            "use zed_cli::nix_export_plan;\n",
            "use zed_cli::nix_export_plan;\nuse zed_cli::oci;\nuse zed_cli::oci_layout;\nuse zed_cli::oci_push::{self, OciPushOptions};\n",
            "main OCI module imports",
        )

    if "if let Cmd::Oci { cmd } = &cli.cmd" not in text:
        old = '''fn run(cli: Cli) -> anyhow::Result<()> {
    let cfg = Config::from_globals(&cli.globals)?;
    let cwd = std::env::current_dir()?;
    if cwd.join(zed_cli::transaction::STAGING_DIR).is_dir() {
'''
        new = '''fn run(cli: Cli) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;

    // OCI planning, local-layout materialization, and ORAS transport
    // intentionally return before Zed registry configuration or project
    // transaction recovery. OCI credentials are always selected explicitly.
    if let Cmd::Oci { cmd } = &cli.cmd {
        return match cmd {
            OciCmd::Plan {
                destination,
                target,
                out,
                json,
            } => {
                if let Some(out) = out {
                    oci_layout::materialize(&cwd, destination, target.as_deref(), out, *json)
                } else {
                    oci::plan(&cwd, destination, target.as_deref(), *json)
                }
            }
            OciCmd::Push {
                layout,
                destination,
                oras,
                username,
                password_stdin,
                registry_config,
                anonymous,
                plain_http,
                insecure_tls,
                ca_file,
                allow_tag_replacement,
                json,
            } => oci_push::push(OciPushOptions {
                layout,
                destination,
                oras,
                username: username.as_deref(),
                password_stdin: *password_stdin,
                registry_config: registry_config.as_deref(),
                anonymous: *anonymous,
                plain_http: *plain_http,
                insecure_tls: *insecure_tls,
                ca_file: ca_file.as_deref(),
                allow_tag_replacement: *allow_tag_replacement,
                interactive: cli.globals.interactive,
                json: *json,
            }),
        };
    }

    let cfg = Config::from_globals(&cli.globals)?;
    if cwd.join(zed_cli::transaction::STAGING_DIR).is_dir() {
'''
        text = replace_once(text, old, new, "main run bootstrap")

    if "Cmd::Oci { .. } =>" not in text:
        anchor = '''        Cmd::Release { cmd } => match cmd {
            ReleaseCmd::Plan { json } => release::plan(&cwd, json),
            ReleaseCmd::Preflight => preflight::preflight(&cwd),
        },
'''
        arm = '''        Cmd::Oci { .. } => {
            unreachable!("OCI commands return before Config construction")
        }
'''
        text = replace_once(text, anchor, anchor + arm, "main OCI dispatch")
    path.write_text(text, encoding="utf-8")


def patch_flags() -> None:
    path = Path(".cli-flags.toml")
    text = path.read_text(encoding="utf-8")
    if "[commands.oci]" in text:
        return
    anchor = '''[commands.release.commands.preflight]
help = "Run fixed credential-free native package preflight adapters."

'''
    block = '''[commands.oci]
help = "Plan, materialize, and distribute immutable Zed packages through OCI registries."

[commands.oci.commands.plan]
help = "Build the exact credential-free OCI publication plan."

[commands.oci.commands.plan.flags.oci_json]
env = "ZED_PKG_OCI_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit the OCI publication plan as JSON."

[commands.oci.commands.push]
help = "Verify a local OCI image layout and copy it to a registry through ORAS."

[commands.oci.commands.push.flags.oci_oras]
env = "ZED_PKG_OCI_ORAS"
aliases = ["oras"]
type = "string"
default = "oras"
help = "ORAS executable path."

[commands.oci.commands.push.flags.oci_username]
env = "ZED_PKG_OCI_USERNAME"
aliases = ["username"]
type = "string"
help = "OCI registry username; pair with --password-stdin."

[commands.oci.commands.push.flags.oci_password_stdin]
env = "ZED_PKG_OCI_PASSWORD_STDIN"
aliases = ["password-stdin"]
type = "bool"
default = "false"
help = "Read one OCI registry password or token from stdin."

[commands.oci.commands.push.flags.oci_registry_config]
env = "ZED_PKG_OCI_REGISTRY_CONFIG"
aliases = ["registry-config"]
type = "string"
help = "Explicit Docker/ORAS registry config path."

[commands.oci.commands.push.flags.oci_anonymous]
env = "ZED_PKG_OCI_ANONYMOUS"
aliases = ["anonymous"]
type = "bool"
default = "false"
help = "Push without registry credentials."

[commands.oci.commands.push.flags.oci_plain_http]
env = "ZED_PKG_OCI_PLAIN_HTTP"
aliases = ["plain-http"]
type = "bool"
default = "false"
help = "Use plain HTTP for a loopback registry only."

[commands.oci.commands.push.flags.oci_insecure_tls]
env = "ZED_PKG_OCI_INSECURE_TLS"
aliases = ["insecure-tls"]
type = "bool"
default = "false"
help = "Disable destination TLS certificate verification."

[commands.oci.commands.push.flags.oci_ca_file]
env = "ZED_PKG_OCI_CA_FILE"
aliases = ["ca-file"]
type = "string"
help = "Custom destination registry CA certificate."

[commands.oci.commands.push.flags.oci_allow_tag_replacement]
env = "ZED_PKG_OCI_ALLOW_TAG_REPLACEMENT"
aliases = ["allow-tag-replacement"]
type = "bool"
default = "false"
help = "Explicitly replace a destination tag whose digest differs."

[commands.oci.commands.push.flags.oci_json]
env = "ZED_PKG_OCI_PUSH_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit the OCI push result as JSON."

'''
    path.write_text(replace_once(text, anchor, anchor + block, "flags OCI block"), encoding="utf-8")


def patch_readme() -> None:
    path = Path("README.md")
    text = path.read_text(encoding="utf-8")
    plan = '| `zed oci plan <oci://registry/repository:version> [--target <name>] [--out <layout>] [--json]` | Derive exact OCI identities and optionally materialize a verified local image layout without credentials or network transport |\n'
    push = '| `zed oci push <layout> <oci://registry/repository:version>` | Verify a local OCI layout, copy it through ORAS using one explicit authentication mode, and require the remote tag to resolve to the expected digest |\n'
    if plan not in text:
        anchor = '| `zed release preflight` | Validate native manifests, then run fixed credential-free package preflight adapters |\n'
        text = replace_once(text, anchor, anchor + plan + push, "README OCI rows")
    elif push not in text:
        text = text.replace(plan, plan + push, 1)
    path.write_text(text, encoding="utf-8")


def main() -> int:
    merge_main()
    patch_cli()
    patch_lib()
    patch_main()
    patch_flags()
    patch_readme()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
