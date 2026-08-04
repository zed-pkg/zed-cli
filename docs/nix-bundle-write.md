# Write a standalone Nix flake bundle

Tracking: DEN-1592, DEN-1418, DEN-1508

`zed interop nix bundle write` composes the frozen Zed → Nix planner, the pure
`zed.nix-flake-bundle/v1` renderer, and the atomic no-clobber persistence API.
It creates one reviewable standalone flake directory from local immutable
inputs.

```bash
zed interop nix bundle write \
  --frozen \
  --flake-lock ./nix/approved-flake.lock \
  --out ./dist/my-package-flake
```

For polyglot packages, select one declared Nix route:

```bash
zed interop nix bundle write \
  --frozen \
  --target rust \
  --flake-lock ./nix/approved-flake.lock \
  --out ./dist/my-package-rust-flake
```

Use `--json` for a compact machine-readable
`zed.nix-flake-bundle-write/v1` receipt. `--output` is a visible compatibility
alias for `--out`.

## Inputs and source of truth

Version 1 requires:

- a valid `.zpkg.toml` at or above the invocation directory;
- the exact existing `.zpkg.lock` selected by `--frozen`;
- artifact-only `[publish.nix]` or `[targets.<target>.nix]` intent accepted by
  the existing strict planner;
- dependency-free, no-build package content accepted by export-plan v1; and
- an explicit approved `flake.lock` with one immutable Nixpkgs revision and
  NAR hash.

The command first asks the existing planner for a canonical
`zed.nix-export-plan/v1`. It then deterministically packs the selected package a
second time and requires filename, format, size, and SHA-256 identity to match
the plan exactly. Only those verified bytes are passed to the pure renderer.
The second pack is an execution-time reproducibility check, not a second
resolver.

## Output and no-clobber policy

The output parent must already exist. A relative basename such as `--out
bundle` is resolved below the current directory before calling the persistence
API.

A missing destination is created atomically. An exact existing bundle returns
`already-current` without rewriting it. Any non-identical existing file,
directory, symlink, file set, byte sequence, or Unix mode fails unchanged.
There is no `--force` or overwrite mode in strict v1.

The generated directory contains:

```text
flake.nix
flake.lock
package.nix
README.md
artifacts/<immutable-zed-artifact>.tar.gz
metadata/plan.json
metadata/bundle.json
```

`metadata/bundle.json` records the plan digest, flake-lock digest, immutable
Nixpkgs identity, sorted file inventory, and domain-separated bundle digest.

## Environment contract

The command supports the same typed flag/environment boundary as Nix planning:

| CLI | Environment |
| --- | --- |
| `--frozen` | `ZED_PKG_FROZEN` |
| `--target` | `ZED_PKG_NIX_TARGET` |
| `--flake-lock` | `ZED_PKG_NIX_FLAKE_LOCK` |
| `--out` / `--output` | `ZED_PKG_NIX_BUNDLE_OUT` |
| `--json` | `ZED_PKG_NIX_PLAN_JSON` |

Boolean values accept `true/false`, `1/0`, `yes/no`, and `on/off`.

## Security boundary

Bundle writing does not:

- read registry, authentication, signing, Cachix, Attic, or cloud credentials;
- resolve or update dependencies;
- update `.zpkg.lock` or `flake.lock`;
- invoke Nix or package build hooks;
- realize a store path;
- upload to an overlay, binary cache, registry, forge, or OCI repository; or
- overwrite caller-owned output.

Nix evaluation and realization remain explicit downstream operations. CI first
acquires the immutable locked closure, then proves `nix flake check --offline`
and `nix build --offline` against the command-produced directory.
