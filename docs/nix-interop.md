# zed-pkg and Nix interoperability

> `zed-pkg` is the independent multi-language package manager implemented by
> this repository. It is not the Zed text editor, its extension system, or an
> editor integration. The shared name is only a naming collision.

This document defines the first executable bridge between a graph locked by
`.zpkg.lock` and a Nix build. It also records the fail-closed design for the
opposite direction, importing a pinned Nix output into zed-pkg.

## Source-of-truth rule

The two systems must not resolve the same graph independently.

For a Zed-to-Nix build:

1. `.zpkg.toml` declares the requested packages.
2. `.zpkg.lock` is the canonical resolved graph and provenance record.
3. `zed install --frozen --install-mode copy` verifies and materializes that
   exact graph.
4. Nix hashes the complete materialized directory recursively.
5. The ordinary package derivation consumes the verified fixed output without
   network access.

Nix therefore verifies the boundary without reimplementing zed-pkg registry,
version, lockfile, artifact, or adapter semantics.

## First bridge: `fetchZedDeps`

`nix/zed-package.nix` exports `fetchZedDeps`, a recursive fixed-output
derivation. Its output contains:

```text
$out/
├── tree/                       # copy-installed project materialization
└── metadata/
    ├── .zpkg.toml              # when a manifest was supplied
    ├── .zpkg.lock              # exact lock used by the build
    ├── contract.json           # normalized adapter boundary
    ├── lock.sha256             # digest of the retained lock
    └── zed-version.txt         # CLI implementation identity
```

The fetch stage:

- requires `.zpkg.lock` and runs only frozen resolution;
- uses copy mode, never global-store links;
- disables dependency build hooks;
- isolates `HOME`, XDG directories, and `ZED_PKG_HOME`;
- removes token and password environment variables;
- accepts only proxy variables as impure networking configuration;
- rejects transaction residue, absolute links, escaping links, and broken
  links; and
- declares `outputHashMode = "recursive"`, `outputHashAlgo = "sha256"`, and
  the caller-provided output hash.

A public registry, immutable `file://` registry, or another credential-free
source is supported by this first implementation. Private registry access is
intentionally not inherited from the caller. It needs a separate, reviewed
secret-delivery contract that cannot leak credentials into derivations, logs,
store paths, or substituter metadata.

### Example

```nix
{ pkgs ? import <nixpkgs> { } }:

let
  zedNix = pkgs.callPackage ./vendor/zed-cli/nix/zed-package.nix { };

  # `zed` is a Nix package containing bin/zed. It may come from a pinned flake,
  # an overlay, or a local package definition.
  zedDeps = zedNix.fetchZedDeps {
    pname = "acme-service-zed-deps";
    version = "1";
    src = ./.;
    zed = pkgs.zed-pkg;
    adapter = "node";
    hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
  };
in
zedNix.mkZedPackage {
  pname = "acme-service";
  version = "1.2.3";
  src = ./.;
  inherit zedDeps;

  nativeBuildInputs = [ pkgs.nodejs ];
  buildPhase = "npm test";
  installPhase = ''
    mkdir -p "$out/share/acme-service"
    cp -a . "$out/share/acme-service/"
  '';
}
```

Start with `lib.fakeHash` or the placeholder above. The first build fails and
prints the actual recursive `sha256-...` value. Record that value in the Nix
expression and rebuild. A later source, lock, adapter, or materialized-output
change then fails until the reviewed hash is updated.

The bridge is also exposed as a flake library:

```nix
inputs.zed-pkg.url = "github:zed-pkg/zed-cli";

# In outputs:
zedNix = inputs.zed-pkg.lib.makeZedPackageLib pkgs;
```

Consumers should pin the zed-cli input in `flake.lock`; the example omits the
rest of the flake only for clarity.

## Ordinary builder: `mkZedPackage`

`mkZedPackage` is a thin `stdenv.mkDerivation` wrapper. It overlays
`zedDeps/tree` into the unpacked source during `postPatch`, then leaves the
normal Nix build surface to the caller. It also exposes the fixed output as
`passthru.zedDeps` and `ZED_PKG_DEPS`.

This is the important trust split:

```text
network-capable fixed output
  zed install --frozen --install-mode copy
                  |
                  | recursive NAR hash
                  v
network-isolated normal derivation
  compile / test / install with Nix inputs
```

Dependency build hooks remain off in the fetch stage because they are arbitrary
package-author code and because their toolchain inputs belong in the normal Nix
derivation. A later adapter may model approved Zed build outputs separately,
but it must not hide compilation inside a dependency downloader.

## What this first bridge is not

The first output is a deterministic **materialized project tree**. It is not yet:

- a snapshot of zed-pkg's global content-addressed store;
- a replacement for a dedicated `zed fetch --frozen --output ...` command;
- an upstream Nixpkgs package definition generator;
- a Zed binary package in a maintained overlay;
- a binary-cache publication service;
- a secret-aware private-registry fetcher; or
- a general translator from arbitrary Nix expressions.

Those distinctions prevent an early proof of concept from becoming an
accidental compatibility contract.

## Publishing Zed packages through Nix

Publishing should be staged in three explicit tiers.

### Tier 1: generated local flake or derivation

`zed nix export` should eventually generate a deterministic package definition
that calls this bridge with:

- immutable zed-cli input identity;
- package name and version;
- supported systems and platform constraints;
- exact recursive dependency-output hash;
- native build and runtime inputs;
- declared outputs and install layout;
- license and source metadata; and
- zed-pkg provenance, signature, SBOM, and attestation references.

Generated files must be stable under repeated export and must never contain a
mutable branch, channel, unqualified registry latest-version lookup, or hidden
credential.

### Tier 2: maintained zed-pkg overlay and binary cache

A project-operated overlay can expose reviewed exports with a cache populated by
CI. Cache publication must bind the Nix output identity to the originating Zed
release, `.zpkg.lock`, source commit/tag, artifact digest, and build attestation.
The overlay and cache are distribution channels, not independent resolvers.

### Tier 3: optional upstream Nixpkgs contribution

An upstream contribution is appropriate only when the package meets Nixpkgs
policy for source availability, licensing, maintainership, supported systems,
build reproducibility, update workflow, and review. Automation may prepare a
candidate patch; it must not submit or merge packages blindly.

## Importing Nix packages into zed-pkg

Nix-to-Zed import is deliberately narrower than "accept a Nix expression". A
safe import starts from a fully locked and evaluated output identity.

A future normalized import descriptor should record at least:

```text
schema version
original flake/reference
locked reference
source revision and narHash
selected attribute path
evaluated system
Nix version
nixpkgs revision
output names
per-output store NAR hashes
platform constraints
license metadata
source metadata
runtime closure policy
provenance and attestations
```

The importer should accept a descriptor generated from a pinned flake lock or an
explicit immutable revision. It should then copy or archive only the selected,
verified outputs into a deterministic Zed artifact and record adapter provenance
in `.zpkg.lock` and registry metadata.

The following inputs must fail closed:

- mutable branches, tags without immutable revision identity, and channels;
- unlocked flake inputs;
- impure evaluation;
- hidden network fetches whose hashes are not known;
- arbitrary functions requiring caller-provided secrets or host state;
- unsupported dynamic overlays or system-dependent attribute selection;
- output sets whose boundaries or hashes cannot be enumerated; and
- platform assumptions that do not match the requested Zed target.

A Nix import is immutable packaging of proven outputs. It does not make the
zed-pkg resolver depend on Nix and it does not require Nix at application
runtime.

## Proposed CLI surface

The final names should be settled with the shared adapter schema, but the
operations should remain explicit:

```text
zed nix export       # .zpkg.lock -> deterministic Nix definition
zed nix publish      # reviewed overlay/cache publication
zed nix import       # pinned Nix descriptor -> immutable Zed package
zed nix verify       # re-evaluate identity and reject drift/tampering
zed fetch --frozen   # future resolver-only materialization primitive
```

`zed fetch` should populate an explicit output/cache directory without project
links, adapters, build hooks, or source-tree mutation. Once shipped, the Nix FOD
can use it instead of extracting the materialized paths from copy installation.
The lockfile remains the only graph-resolution input in either implementation.

## Platform boundary

The canary exercises Linux and macOS. Windows support should be defined through
WSL or a future native Nix implementation; the bridge must not imply that a
Linux or Darwin store output is portable to native Windows. Imported and
exported metadata must always include the evaluated system and output platform.

## CI contract

`.github/workflows/nix-interop.yml` proves the current boundary by:

1. building the exact zed-cli branch;
2. publishing the existing Node fixture to a local registry;
3. adding that registry to the Nix store before resolution so the frozen lock
   points at an immutable, sandbox-visible URL;
4. generating a real `.zpkg.lock` and uninstalling the project materialization;
5. evaluating the standalone flake library;
6. bootstrapping the recursive fixed-output hash;
7. rebuilding the same graph under two derivation names and comparing NAR hashes;
8. building and checking a normal offline consumer derivation;
9. rejecting an incorrect Nix output hash; and
10. rejecting a lockfile whose Zed artifact digest was tampered with.

This canary is intentionally based on the same copy-install fixture used for OCI
ownership tests, so Nix and container boundaries enforce one materialization
contract instead of drifting into separate implementations.
