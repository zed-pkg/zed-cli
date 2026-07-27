#!/usr/bin/env python3
"""Wire the DEN-100 release-plan command into Clap, flags2env, and docs."""

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
    '''    /// Build the pruned, deterministic artifact for this package
    Pack {
        #[arg(long, env = "ZED_PKG_PACK_OUT")]
        out: Option<PathBuf>,
    },
    /// Pack, verify VCS tag provenance, and upload to the registry
    Publish {''',
    '''    /// Build the pruned, deterministic artifact for this package
    Pack {
        #[arg(long, env = "ZED_PKG_PACK_OUT")]
        out: Option<PathBuf>,
    },
    /// Plan a coordinated Zed + native-registry release without credentials or uploads
    Release {
        #[command(subcommand)]
        cmd: ReleaseCmd,
    },
    /// Pack, verify VCS tag provenance, and upload to the registry
    Publish {''',
    "Cmd::Release",
)

replace_once(
    "src/cli.rs",
    '''#[derive(Debug, Subcommand)]
pub enum AuthCmd {''',
    '''#[derive(Debug, Subcommand)]
pub enum ReleaseCmd {
    /// Print the deterministic release set derived from `.zpkg.toml`
    Plan {
        /// Emit machine-readable JSON rather than the human summary
        #[arg(long, env = "ZED_PKG_RELEASE_JSON")]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCmd {''',
    "ReleaseCmd enum",
)

replace_once(
    ".cli-flags.toml",
    '''[flags.out]
env = "ZED_PKG_PACK_OUT"
aliases = ["out"]
type = "string"
help = "Packed artifact output path."

[flags.allow_dirty]''',
    '''[flags.out]
env = "ZED_PKG_PACK_OUT"
aliases = ["out"]
type = "string"
help = "Packed artifact output path."

[flags.release_json]
env = "ZED_PKG_RELEASE_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit a machine-readable release plan."

[flags.allow_dirty]''',
    "release_json flag",
)

replace_once(
    ".cli-flags.toml",
    '''[commands.pack]
help = "Pack an artifact."

[commands.publish]''',
    '''[commands.pack]
help = "Pack an artifact."

[commands.release]
help = "Coordinate Zed and native-registry releases."

[commands.release.commands.plan]
help = "Print a credential-free deterministic release plan."

[commands.release.commands.plan.flags.release_json]
env = "ZED_PKG_RELEASE_JSON"
aliases = ["json"]
type = "bool"
default = "false"
help = "Emit the release plan as JSON."

[commands.publish]''',
    "release flags2env command",
)

replace_once(
    "README.md",
    '''| `zed pack` | Build the pruned, deterministic `tar.gz` artifact |
| `zed publish` | Verify clean tree + matching VCS tag at HEAD, pack, upload |''',
    '''| `zed pack` | Build the pruned, deterministic `tar.gz` artifact |
| `zed release plan [--json]` | Print the credential-free Zed + native-registry release set derived from `.zpkg.toml` |
| `zed publish` | Verify clean tree + matching VCS tag at HEAD, pack, upload |''',
    "README command table",
)

print("wired release plan command")
