# zed-pkg Nix bridge

This directory contains the first reproducible package boundary between the
independent `zed-pkg` package manager and Nix.

- `zed-package.nix` exports `fetchZedDeps` and `mkZedPackage`.
- `default.nix` exposes the library through `callPackage`.
- `flake.nix` exposes `lib.makeZedPackageLib` for pinned flake consumers.

The fetcher consumes an existing `.zpkg.lock`, runs frozen copy installation in
a recursive fixed-output derivation, and emits a materialized tree plus retained
provenance metadata. The normal builder consumes that verified output offline.

See [`docs/nix-interop.md`](../docs/nix-interop.md) for the trust model, usage,
publishing tiers, Nix-to-Zed import boundary, and CI contract.
