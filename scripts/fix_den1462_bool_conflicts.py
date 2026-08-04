#!/usr/bin/env python3
"""Avoid Clap presence-conflicts caused by flags-2-env boolean defaults.

flags-2-env exports explicit false defaults for both mode flags. Clap correctly
parses those values as false, but `conflicts_with` reasons about argument
presence rather than the parsed boolean value. Validate the two true booleans
at the dispatcher boundary instead, where CLI and environment inputs have
already been normalized.
"""

from pathlib import Path


def replace_once(path: Path, old: str, new: str, label: str) -> None:
    text = path.read_text(encoding="utf-8")
    if new in text:
        return
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one anchor, found {count}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


cli = Path("src/cli.rs")
replace_once(
    cli,
    '#[arg(long, env = "ZED_PKG_ENV_CHECK", conflicts_with = "write")]',
    '#[arg(long, env = "ZED_PKG_ENV_CHECK")]',
    "check flag presence conflict",
)
replace_once(
    cli,
    '#[arg(long, env = "ZED_PKG_ENV_WRITE", conflicts_with = "check")]',
    '#[arg(long, env = "ZED_PKG_ENV_WRITE")]',
    "write flag presence conflict",
)
replace_once(
    cli,
    "    use super::{AuthCmd, Cli, Cmd};\n",
    "    use super::{AuthCmd, Cli, Cmd, EnvCmd};\n",
    "export command test import",
)
replace_once(
    cli,
    '''        assert!(Cli::try_parse_from([
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
''',
    '''        assert!(matches!(
            Cli::try_parse_from([
                "zed",
                "env",
                "export",
                "mise",
                "--plan",
                "zed-env.toml",
                "--check",
                "--write",
            ])
            .unwrap()
            .cmd,
            Cmd::Env {
                cmd: EnvCmd::Export {
                    check: true,
                    write: true,
                    ..
                }
            }
        ));
''',
    "typed export ambiguity parser regression",
)

main = Path("src/main.rs")
replace_once(
    main,
    '''            } => {
                let mode = if check {
''',
    '''            } => {
                if check && write {
                    anyhow::bail!("the arguments '--check' and '--write' cannot be used together");
                }
                let mode = if check {
''',
    "runtime export mode conflict",
)
