# Project-local asdf interoperability

`zed-asdf` is the executable first slice of the asdf adapter. It is deliberately
separate from the main `zed env` dispatcher until the shared mise command branch
lands; the reusable implementation lives in `zed_cli::asdf_environment` so the
later wiring step does not need to reimplement parsing or validation.

## Commands

```text
zed-asdf import [--config PATH] [--lock PATH] [--frozen] [--json]
zed-asdf verify [--config PATH] [--lock PATH] [--frozen] [--json]
```

The default inputs are the current project's `.tool-versions` and the optional
Zed-owned `.zed/asdf.lock.toml`. The adapter never invokes `asdf`, searches
parents, reads `$HOME`, installs plugins, evaluates plugin scripts, or mutates
either file.

## Why a sidecar is required for frozen verification

`.tool-versions` selects tool names and versions, but it does not identify the
plugin repository commit or the downloaded artifact bytes. Frozen portable
verification therefore requires a committed sidecar:

```toml
schema = 1

[plugins.nodejs]
version = "22.11.0"
url = "https://github.com/asdf-vm/asdf-nodejs.git"
revision = "0123456789abcdef0123456789abcdef01234567"
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
platforms = ["aarch64-darwin", "x86_64-linux"]
```

Every configured plugin must have exactly one matching entry, and the sidecar
may not contain extras. The plugin URL must be portable and credential-free, the
revision must be a full immutable 40- or 64-character hexadecimal commit, and
the tool artifact requires a lowercase-normalized SHA-256. Platform identity is
stored separately rather than encoded in SemVer build metadata.

## Supported initial subset

- one project-local `.tool-versions`;
- exactly one selected version per plugin;
- exact config/lock tool-name coverage;
- exact version equality between config and sidecar;
- immutable plugin URL and commit provenance;
- SHA-256 tool artifact identity;
- optional platform constraints;
- canonical semantic input and `EnvironmentPlan` digests.

Multiple asdf fallback versions are rejected because `EnvironmentPlan` v1 has
one resolved identity per tool. `system`, mutable refs, local paths, and other
moving selectors fail shared frozen-portable validation.

## Isolation and tests

The adapter is parser-only and read-only. Integration tests launch the real
`zed-asdf` binary with an empty `PATH`, verify that no ambient asdf executable is
needed, snapshot both input files, and prove that parent `.tool-versions` files
are not inherited. The dedicated workflow runs the same contract on Linux,
macOS, and Windows with immutable Action pins.
