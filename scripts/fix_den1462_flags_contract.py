#!/usr/bin/env python3
"""Move mise-export flags into the nested flags2env command scope."""

from pathlib import Path

path = Path(".cli-flags.toml")
text = path.read_text(encoding="utf-8")

root_blocks = """
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
if root_blocks in text:
    text = text.replace(root_blocks, "", 1)

command = """[commands.env.commands.export]
help = "Project a schema-v2 EnvironmentPlan into deterministic manager configuration."
"""
scoped = command + """

[commands.env.commands.export.flags.env_plan]
env = "ZED_PKG_ENV_PLAN"
aliases = ["plan"]
type = "string"
help = "Project-local schema-v2 environment plan path."

[commands.env.commands.export.flags.env_output]
env = "ZED_PKG_ENV_OUTPUT"
aliases = ["output"]
type = "string"
default = ".mise.toml"
help = "Project-local manager export path."

[commands.env.commands.export.flags.env_check]
env = "ZED_PKG_ENV_CHECK"
aliases = ["check"]
type = "bool"
default = "false"
help = "Verify the deterministic manager projection without writing."

[commands.env.commands.export.flags.env_write]
env = "ZED_PKG_ENV_WRITE"
aliases = ["write"]
type = "bool"
default = "false"
help = "Transactionally write a Zed-owned manager projection."
"""
if scoped not in text:
    if text.count(command) != 1:
        raise SystemExit("env export command scope not found exactly once")
    text = text.replace(command, scoped, 1)

path.write_text(text, encoding="utf-8")
