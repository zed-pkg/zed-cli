# Revision-pinned Git CLI installs

Zed can install a repository-owned Rust CLI from an immutable Git revision using the same core inputs as `cargo install --git ... --rev ... --bin ... --force`.

The first implementation is exposed through the shipped `zed-git-install` extension, which the `zed` external-command dispatcher resolves as `zed git-install` when both executables are installed together:

```bash
zed git-install \
  --git https://github.com/ORESoftware/ores-cli.git \
  --rev 387bce152d9572c014710d68062f979c3614276d \
  --bin ores-cli \
  --force
```

The installer deliberately uses the system `git` executable for fetch/checkout and sets `CARGO_NET_GIT_FETCH_WITH_CLI=true` for the Cargo build. `--rev` must be a full 40-character SHA-1 or 64-character SHA-256 commit id; branches and tags are rejected so the receipt always names immutable source provenance.

## Repository contract

The source repository must contain `.zpkg.toml` and declare the selected executable in `[bin]`:

```toml
[package]
language = "rust"

[bin]
ores-cli = "target/release/ores-cli"

[cli]
flags_contract = ".cli-flags.toml"
flags_runtime = "flags-2-env"
primary_bin = "ores-cli"
```

When `[cli].flags_contract` is declared, installation fails closed unless the file is present, regular (not a symlink), and passes the bundled flags-2-env audit. For compatibility with existing CLI repositories, a root `.cli-flags.toml` is also audited automatically when present even before the explicit `[cli]` metadata is added.

## Install location and PATH

On macOS/Linux the default global executable directory is:

```text
~/.local/bin
```

On Windows it is the `zed-pkg/bin` directory below the user's local application-data directory. Override either with `--global-bin-dir` or `ZED_PKG_GLOBAL_BIN_DIR`.

After installation Zed prints PATH guidance. On Unix-like shells this is equivalent to:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

The source revision, selected binary, installed path, flags contract, and SHA-256 of the activated executable are recorded below `~/.zed-pkg/global/git-installs/` (or `ZED_PKG_HOME`).

## Replacement semantics

Without `--force`, installation refuses to overwrite an existing executable. With `--force`, the newly built executable is completely fetched, revision-verified, manifest-validated, flags-contract-audited, built, and staged before Zed replaces the existing destination. A backup is retained until activation succeeds.

This source-install path is separate from `zed install --cli nodejs` / `python3`, which installs project-owned runtime toolchains rather than repository-owned application binaries.

Future command-surface work can make `zed install --git ...` a direct alias of this exact code path; the source/install semantics should remain unchanged.
