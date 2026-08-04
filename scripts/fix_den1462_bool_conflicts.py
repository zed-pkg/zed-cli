#!/usr/bin/env python3
"""Finish current-main mise export integration after env normalization.

flags-2-env exports explicit false defaults for both mode flags. Clap parses
those values correctly, but `conflicts_with` reasons about argument presence.
The current dispatcher also supports both mise and asdf import/verify routes,
so this repair inserts export semantically without replacing either route.
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
text = main.read_text(encoding="utf-8")
if "            EnvCmd::Export {" not in text:
    verify_anchor = '''            EnvCmd::Verify {
                manager,
                config,
                lock,
                frozen,
                json,
            } => match manager {
'''
    export_dispatch = '''            EnvCmd::Export {
                manager: EnvironmentManagerArg::Mise,
                plan,
                output,
                check,
                write,
                json,
            } => {
                if check && write {
                    anyhow::bail!("the arguments '--check' and '--write' cannot be used together");
                }
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
            EnvCmd::Export {
                manager: EnvironmentManagerArg::Asdf,
                ..
            } => anyhow::bail!(
                "asdf export is not implemented; use `zed env export mise`"
            ),
''' + verify_anchor
    count = text.count(verify_anchor)
    if count != 1:
        raise SystemExit(f"current-main export dispatcher: expected one anchor, found {count}")
    main.write_text(text.replace(verify_anchor, export_dispatch, 1), encoding="utf-8")
else:
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
