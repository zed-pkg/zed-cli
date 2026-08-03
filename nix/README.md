# zed-pkg Nix bridge

This directory contains the first reproducible package boundary between the
independent `zed-pkg` package manager and Nix.

- `zed-package.nix` exports `fetchZedDeps` and `mkZedPackage`.
- `default.nix` exposes the library through `callPackage`.
- `flake.nix` exposes `lib.makeZedPackageLib` for pinned flake consumers and the
  locked Nixpkgs path used by CI.
- `flake.lock` pins the reviewed Nixpkgs baseline shared with the current
  `zed-interfaces` Nix contract work.

The fetcher consumes an existing `.zpkg.lock`, runs frozen copy installation in
a recursive fixed-output derivation, and emits a materialized tree plus retained
lock evidence and self-identified `zed.nix-fetch-bridge/v1` metadata. That bridge
receipt is deliberately not the canonical `zed.nix-adapter/v1` publication
record. The normal builder consumes the verified output offline.

See [`docs/nix-interop.md`](../docs/nix-interop.md) for the trust model, usage,
publishing tiers, Nix-to-Zed import boundary, and CI contract.
