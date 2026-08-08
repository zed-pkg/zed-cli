#!/usr/bin/env python3
from pathlib import Path

path = Path("src/external_subcommands.rs")
text = path.read_text(encoding="utf-8")
replacements = [
    (
        "pub fn augment_root_command(mut command: ClapCommand) -> ClapCommand {",
        "pub fn augment_root_command(command: ClapCommand) -> ClapCommand {",
    ),
    (
        "fn root_value_option<'a>(token: &'a str) -> Option<(&'static str, Option<&'a str>)> {",
        "fn root_value_option(token: &str) -> Option<(&'static str, Option<&str>)> {",
    ),
    (
        '''    if let Some(directory) = sibling.filter(|directory| directory.is_absolute()) {
        if let Some(executable) = executable_in(directory, &stem) {
            return Some(executable);
        }
    }
''',
        '''    if let Some(executable) = sibling
        .filter(|directory| directory.is_absolute())
        .and_then(|directory| executable_in(directory, &stem))
    {
        return Some(executable);
    }
''',
    ),
    (
        "        return status.signal().map(|signal| 128 + signal).unwrap_or(1);",
        "        status.signal().map(|signal| 128 + signal).unwrap_or(1)",
    ),
]
for old, new in replacements:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one clippy replacement target, found {count}: {old[:80]!r}")
    text = text.replace(old, new, 1)
path.write_text(text, encoding="utf-8")
