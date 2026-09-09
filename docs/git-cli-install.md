# Revision-pinned Git CLI installation

Zed supports Cargo-style installation of a repository-owned CLI from one exact
Git commit without conflating that operation with project dependency installs or
project-owned tool runtimes.

```sh
zed install \
  --git https://github.com/ORESoftware/ores-cli.git \
  --rev 387bce152d9572c014710d68062f979c3614276d \
  --bin ores-cli \
  --force
```

`--rev` is intentionally immutable: the installer accepts only a full
40-character SHA-1 or 64-character SHA-256 Git object ID. Branch names, tags,
and abbreviated revisions are refused.

## Contract boundary

A source-install candidate must own a root `.zpkg.toml`. `--bin` must name an
entry in that manifest's `[bin]` table and the source checkout must resolve to
exactly the requested commit before build or activation.

If the package declares `[cli].flags_contract`, that repository-relative file
is mandatory. Otherwise a root `.cli-flags.toml` is discovered when present.
The contract must be a regular repository-owned file and is audited fail-closed
with the official `flags-2-env` runtime before the package is built.

The install receipt is the shared `GitCliInstallReceiptV1` contract from
`zed-pkg/zed-interfaces`. Its wire shape has two independent authorities:

- TypeSpec: `schema-authority-canary/git-cli-install-v1/main.tsp`
- authored JSON Schema Draft 2020-12:
  `schema-authority-canary/git-cli-install-v1/authored.schema.json`

Neither source is generated from or subordinate to the other. CI compares their
semantics and consumer-admission behavior with
`ORESoftware/typespec-json-schema-validator`; generated JSON Schema, Contract
IR, SARIF, and other witnesses are retained as evidence only.

## Installation transaction

For Rust source packages the current implementation:

1. creates an isolated temporary checkout;
2. fetches only the requested immutable commit and verifies `HEAD` exactly;
3. validates `.zpkg.toml`, the requested `[bin]`, and the target CLI flags
   contract;
4. builds the requested executable with `cargo build --release --locked --bin`;
5. stages the executable in the destination filesystem;
6. atomically activates it without clobbering an existing entry unless
   `--force` was requested;
7. hashes the activated executable with SHA-256; and
8. writes a validated shared install receipt below
   `$ZED_PKG_HOME/global/git-installs/`.

A failed build, contract audit, validation, or activation does not intentionally
remove the previously installed executable.

## Global executable directory

`--global-bin-dir PATH` (or `ZED_PKG_GLOBAL_BIN_DIR`) overrides the stable user
executable directory. Without an override, Zed uses:

- Unix: `~/.local/bin`
- Windows: `%LOCALAPPDATA%\\zed-pkg\\bin`

Windows installs use the `.exe` executable suffix while the manifest keeps a
platform-neutral `[bin]` identity.

## Separation from project-owned runtimes

This source-install route is deliberately separate from:

```sh
zed install --cli nodejs
zed install --cli python3
```

Those commands materialize project-owned tool runtimes under `.zed/tools` and
participate in `EnvironmentLock`. Revision-pinned `--git` installation instead
creates a user-level executable and a global Git-install receipt. Zed rejects a
single invocation that mixes the two models.

## Credential handling

Repository URLs must not embed HTTPS credentials. Git authentication, when
required, is delegated to the user's normal Git credential/SSH configuration;
secrets are not accepted as source-install command-line flags.
