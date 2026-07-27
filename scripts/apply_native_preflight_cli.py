#!/usr/bin/env python3
"""Wire the fixed native preflight adapters into the zed CLI."""

from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one insertion point, found {count}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/cli.rs",
    '''pub enum ReleaseCmd {
    /// Print the deterministic release set derived from `.zpkg.toml`
    Plan {
        /// Emit machine-readable JSON rather than the human summary
        #[arg(long, env = "ZED_PKG_RELEASE_JSON")]
        json: bool,
    },
}''',
    '''pub enum ReleaseCmd {
    /// Print the deterministic release set derived from `.zpkg.toml`
    Plan {
        /// Emit machine-readable JSON rather than the human summary
        #[arg(long, env = "ZED_PKG_RELEASE_JSON")]
        json: bool,
    },
    /// Run fixed, credential-free npm and crates.io package preflight adapters
    Preflight,
}''',
    "ReleaseCmd::Preflight",
)

replace_once(
    "src/main.rs",
    '''use zed_cli::ops;
use zed_cli::r2g::{self, R2gOptions};''',
    '''use zed_cli::ops;
use zed_cli::preflight;
use zed_cli::r2g::{self, R2gOptions};''',
    "preflight import",
)

replace_once(
    "src/main.rs",
    '''        Cmd::Release { cmd } => match cmd {
            ReleaseCmd::Plan { json } => release::plan(&cwd, json),
        },''',
    '''        Cmd::Release { cmd } => match cmd {
            ReleaseCmd::Plan { json } => release::plan(&cwd, json),
            ReleaseCmd::Preflight => preflight::preflight(&cwd),
        },''',
    "preflight dispatch",
)

replace_once(
    ".cli-flags.toml",
    '''[commands.release.commands.plan.flags.release_json]
env = "ZED_PKG_RELEASE_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit the release plan as JSON."

[commands.publish]''',
    '''[commands.release.commands.plan.flags.release_json]
env = "ZED_PKG_RELEASE_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit the release plan as JSON."

[commands.release.commands.preflight]
help = "Run fixed credential-free native package preflight adapters."

[commands.publish]''',
    "preflight flags2env command",
)

replace_once(
    "README.md",
    '''| `zed release plan [--json]` | Print the credential-free Zed + native-registry release set derived from `.zpkg.toml` |
| `zed publish` | Verify clean tree + matching VCS tag at HEAD, pack, upload |''',
    '''| `zed release plan [--json]` | Print the credential-free Zed + native-registry release set derived from `.zpkg.toml` |
| `zed release preflight` | Validate native manifests, then run fixed credential-free npm/crates.io package preflight adapters |
| `zed publish` | Verify clean tree + matching VCS tag at HEAD, pack, upload |''',
    "README preflight command",
)

replace_once(
    "src/preflight.rs",
    '''    use std::cell::RefCell;
    use std::os::unix::process::ExitStatusExt;

    use super::*;''',
    '''    use std::cell::RefCell;
    use std::process::ExitStatus;

    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    use super::*;

    fn exit_status(success: bool) -> ExitStatus {
        #[cfg(unix)]
        {
            ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
        }
        #[cfg(windows)]
        {
            ExitStatus::from_raw(if success { 0 } else { 1 })
        }
    }''',
    "portable ExitStatus helper",
)

replace_once(
    "src/preflight.rs",
    '''                status: if success {
                    std::process::ExitStatus::from_raw(0)
                } else {
                    std::process::ExitStatus::from_raw(1 << 8)
                },''',
    '''                status: exit_status(success),''',
    "fake command status",
)

print("wired native preflight adapters")
