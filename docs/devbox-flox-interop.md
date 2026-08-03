# Deterministic Devbox and Flox exports

`zed-env-export` is the staged executable for the first DEN-1468 export slice.
It consumes the shared, frozen `EnvironmentPlan` and emits manager-native
configuration without invoking Devbox, Flox, Nix, package resolution, or any
activation hook.

The module is kept separate from the moving `zed env` dispatcher so it can be
wired into `zed env export devbox|flox` after the common command branch lands
without reimplementing generation.

## Commands

```text
zed-env-export devbox [--plan PATH] [--out PATH] [--receipt PATH] [--json]
zed-env-export flox   [--plan PATH] [--out PATH] [--receipt PATH] [--json]
```

Defaults:

| Manager | Output | Receipt |
| --- | --- | --- |
| Devbox | `devbox.json` | `.zed/environment-exports/devbox.json` |
| Flox | `.flox/env/manifest.toml` | `.zed/environment-exports/flox.json` |

The default input is `.zed/environment-plan.json`. Every path must be normalized,
project-relative, and free of symlink traversal outside the project.

## Mapping contract

The initial exporter supports explicit Nixpkgs-backed entries only. It never
guesses that a language name maps one-to-one to a Nix attribute.

A tool uses:

```json
{
  "requirement": "^22",
  "resolved": "22.11.0",
  "provider": "nixpkgs",
  "backend": "nodejs_22"
}
```

A system package uses:

```json
{
  "requirement": "2.47.0",
  "resolved": "2.47.0",
  "provider": "nixpkgs",
  "package_ref": "gitFull"
}
```

The whole plan must pass `FrozenPortable` validation and set activation to
`frozen-install`. Resolved identities therefore cannot be absent, local, or
moving selectors. Package references contain no version suffix: versions and
platforms are emitted as separate fields.

## Devbox output

The generator writes the object form of `packages`, with one exact version and
an optional platform list per Nixpkgs package. The only shell hook is:

```text
zed install --frozen
```

The Zed dependency graph is never copied into the Devbox package list. Devbox
provides toolchains and system packages around the already locked Zed install.

## Flox output

The generator emits manifest schema version 1, deterministic `[install]`
entries with `pkg-path`, `version`, and optional `systems`, an optional global
`[options].systems`, and the same fixed activation command under
`[hook].on-activate`.

The initial Flox platform subset is the common Darwin/Linux matrix:

- `aarch64-darwin`
- `aarch64-linux`
- `x86_64-darwin`
- `x86_64-linux`

Unsupported platform identities fail before any file is written.

## Receipts and native locks

Manager files cannot safely carry Zed-only unknown fields, so provenance is
stored in a deterministic Zed-owned receipt. It records:

- raw input SHA-256;
- canonical normalized `EnvironmentPlan` SHA-256;
- output SHA-256;
- project-relative input and output paths;
- generator schema and manager;
- exact package mapping, platforms, and per-entry provenance digest; and
- `native_lock_required = true`.

The receipt does **not** pretend to be `devbox.lock` or Flox `manifest.lock`.
Manager-native lock generation, offline replay, and lock-digest attachment are a
separate validation step that must run with the actual manager binary.

## Conflict and write policy

Generation is byte-deterministic. Repeating an identical export succeeds
without rewriting either file. A differing existing output or receipt is
considered human-owned or drifted and is never overwritten.

When both paths are absent, each file is written through a same-directory
temporary file with no-clobber persistence. All conflicts are preflighted before
writes; if receipt persistence fails after a new output was installed, the new
output is removed so the command fails closed.

Semantic merging of pre-existing human manager configuration remains a later
adapter phase. This first PR chooses explicit conflict diagnostics rather than
silently deleting scripts, services, includes, environment variables, or other
manager-owned state.
