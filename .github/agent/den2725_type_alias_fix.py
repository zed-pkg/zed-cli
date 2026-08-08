#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement target, found {count}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/external_subcommands.rs",
    '''const ROOT_BOOLEAN_OPTIONS: &[(&str, &str)] = &[
    ("--interactive", "ZED_PKG_INTERACTIVE"),
    ("--git-submodules", "ZED_PKG_GIT_SUBMODULES"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
''',
    '''const ROOT_BOOLEAN_OPTIONS: &[(&str, &str)] = &[
    ("--interactive", "ZED_PKG_INTERACTIVE"),
    ("--git-submodules", "ZED_PKG_GIT_SUBMODULES"),
];

type ExternalEnvironment = Vec<(OsString, OsString)>;
type ParsedExternalArguments = (Vec<OsString>, ExternalEnvironment);

#[derive(Debug, Clone, PartialEq, Eq)]
''',
)

replace_once(
    "src/external_subcommands.rs",
    "    environment: Vec<(OsString, OsString)>,\n",
    "    environment: ExternalEnvironment,\n",
)

replace_once(
    "src/external_subcommands.rs",
    '''fn extract_root_options(
    args: &[OsString],
) -> Option<(Vec<OsString>, Vec<(OsString, OsString)>)> {
''',
    '''fn extract_root_options(args: &[OsString]) -> Option<ParsedExternalArguments> {
''',
)
