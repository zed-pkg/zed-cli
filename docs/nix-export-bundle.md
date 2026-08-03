# Deterministic standalone Nix export bundles

This document defines the executable rendering boundary between a frozen
`zed.nix-export-plan/v1` and a standalone Nix flake. In this document, **Zed**
means the independent `zed-pkg` multi-language package manager, not the Zed text
editor.

## Purpose

The planner decides whether a package is eligible for strict artifact-only Nix
export. The renderer turns that already-reviewed plan into deterministic files.
It is deliberately pure: it receives a plan, exact artifact bytes, and an
approved `flake.lock`, then returns an in-memory path-to-bytes map.

Rendering does not write files, evaluate Nix, realize a store path, access a
registry, read credentials, consult `$HOME`, update a lock, publish a cache, or
create a final adapter attestation. Those operations remain explicit later
stages.

## Input authority

The renderer accepts exactly three inputs:

1. a validated, dependency-free `zed.nix-export-plan/v1`;
2. the exact canonical `tar.gz` artifact named and hashed by that plan; and
3. a version-7 Nix flake lock whose sole input is an exact
   `github:NixOS/nixpkgs/<revision>` selector with matching locked revision and
   SHA-256 NAR hash.

The Zed plan remains the package-resolution authority. The generated flake does
not contain a semver resolver, registry lookup, build-command inference, or
implicit system/output selection.

## Version-1 output

The renderer produces these files:

```text
flake.nix
flake.lock
package.nix
README.md
artifacts/<org>-<name>-<version>.tar.gz
metadata/plan.json
metadata/bundle.json
```

`flake.nix` exposes the declared attribute and a `default` alias for every
explicit system. `package.nix` unpacks the immutable Zed archive, installs the
payload under `share/zed-pkg/<org>/<name>/<version>`, and creates `$out/bin`
symlinks only for prebuilt files that were executable inside the archive.

The renderer preserves the supplied `flake.lock` bytes exactly. It does not run
`nix flake update`, infer Nixpkgs, or accept a mutable branch selector.

## Canonical bundle inventory

`metadata/bundle.json` uses schema:

```text
zed.nix-flake-bundle/v1
```

It records:

- the canonical export-plan SHA-256;
- the exact `flake.lock` SHA-256;
- the immutable Nixpkgs reference, revision, and NAR hash;
- a path-sorted list of every generated file except the inventory itself; and
- one domain-separated bundle SHA-256.

The bundle digest is computed over the schema domain followed by, for each
sorted entry, the big-endian path length, UTF-8 path bytes, big-endian file
size, and raw 32-byte SHA-256 digest. It does not hash filesystem ownership,
permissions, mtimes, output-directory names, archive ordering, or host paths.
This avoids both self-reference and platform-dependent directory archives.

## Fail-closed validation

Version 1 rejects:

- artifact hash or size drift;
- ZIP or unknown artifact formats;
- dependencies, source-build inference, or outputs other than exactly `out`;
- unsorted system/output declarations;
- noncanonical artifact filenames;
- unsafe package, version, target, executable, payload, or generated paths;
- traversal, absolute paths, backslashes, duplicate archive paths, symlinks,
  hardlinks, devices, and other non-regular archive entries;
- missing, empty, or non-executable declared binaries;
- excessive archive entry counts or declared unpacked size;
- mutable, multi-input, unsupported-version, or structurally ambiguous flake
  locks; and
- post-render mutation of any inventoried file.

The metadata is source-redacted: it does not add Zed registry URLs, tokens,
workspace paths, usernames, hostnames, cache paths, or timestamps. The package
payload remains the package payload; the renderer does not claim to remove
secrets that a publisher intentionally placed inside source artifacts.

## Realization and final provenance

An operator or later CLI layer persists the returned map atomically, then may
perform:

```console
nix flake archive --no-update-lock-file
nix flake check --offline --no-update-lock-file
nix build --offline --no-update-lock-file '.#<attribute>'
```

Network access is allowed only as an explicit preparation step to obtain the
already-pinned Nixpkgs input. Evaluation and build certification run offline
with lock updates disabled.

`zed.nix-flake-bundle/v1` is not `zed.nix-adapter/v1`. A final adapter record is
created only after each declared system/output has realized evidence such as
derivation JSON digest, NAR hash and size, references, signatures, Nix version,
and store-info JSON version.

## Review and merge order

1. merge the canonical export-plan schema;
2. merge the read-only frozen planner;
3. merge this pure renderer and its Linux/macOS certification;
4. add atomic filesystem persistence and explicit Nix realization;
5. add final adapter-record construction and verification; and
6. only then integrate reviewed overlays, binary caches, or Nixpkgs submission.

Linear: DEN-1508, child of DEN-1418.
