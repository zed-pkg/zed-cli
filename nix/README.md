# zed-pkg Nix bridge

This directory contains reproducible package boundaries between the independent
`zed-pkg` package manager and Nix.

- `zed-package.nix` exports the install-shaped `fetchZedDeps` and
  `mkZedPackage` helpers.
- `zed-fetch-bundle.nix` exports `fetchZedArtifacts`, a resolver-only
  `zed.fetch/v1` bundle FOD.
- `default.nix` combines both bridge families without changing their contracts.
- `flake.nix` exposes `lib.makeZedPackageLib` for pinned flake consumers and the
  locked Nixpkgs path used by CI.
- `flake.lock` pins the reviewed Nixpkgs baseline shared with the merged
  `zed-interfaces` Nix intent, provenance, and schema contract.

## Install-shaped bridge

`fetchZedDeps` consumes an existing `.zpkg.lock`, runs frozen copy installation
in a recursive fixed-output derivation, and emits a materialized tree plus exact
lock/manifest digests, a source-redacted package inventory, and self-identified
`zed.nix-fetch-bridge/v1` metadata. An immutable `registryPath` remains an
explicit Nix input and is addressed through its exact store identity, so the
`file://` URL already frozen into `.zpkg.lock` does not drift during the sandbox
build. Raw lock, manifest, and registry URL text are intentionally not retained
in the FOD output: Nix fixed outputs must not reference other store objects, and
registry strings can also carry credentials. The bridge receipt is deliberately
not the canonical `zed.nix-adapter/v1` publication record. `mkZedPackage`
consumes this verified installation tree offline.

## Resolver-only artifact bridge

`fetchZedArtifacts` runs `zed fetch --frozen` instead of project installation.
Its fixed output contains content-addressed package payloads and source-redacted
`zed.fetch/v1` metadata, with no adapters, project references, dependency build
hooks, raw lock, manifest, or registry literal. This is the lower-level boundary
for external builders, caches, exporters, and a future explicit offline
materializer.

The two helpers are intentionally not interchangeable. Use `fetchZedDeps` when
a normal Nix derivation needs an installation-shaped project tree. Use
`fetchZedArtifacts` when the caller needs verified package content without
project mutation.

See:

- [`docs/nix-interop.md`](../docs/nix-interop.md) for the overall trust model,
  publishing tiers, and Nix-to-Zed boundary;
- [`docs/nix-fetch-bundle.md`](../docs/nix-fetch-bundle.md) for resolver-only FOD
  usage, output format, and acceptance gates; and
- [`docs/frozen-fetch.md`](../docs/frozen-fetch.md) for the underlying CLI
  contract.
