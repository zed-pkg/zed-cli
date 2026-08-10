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
    ├── bridge.json             # operational FOD bridge metadata
    ├── lock-summary.json       # stable package inventory; sources redacted
    ├── lock.sha256             # SHA-256 of the exact input lock bytes
    ├── manifest.sha256         # present when a manifest was supplied
    └── zed-version.txt         # CLI implementation identity
```

The raw `.zpkg.lock` and `.zpkg.toml` remain derivation inputs. They are not
copied into the fixed output. This is an important Nix boundary: a valid frozen
lock may name an immutable `file:///nix/store/...` registry, but a fixed-output
derivation must not retain references to another store object. The bridge
therefore verifies the original bytes before and after installation, records
their exact digests, and emits a deterministic package inventory that retains
artifact identity while replacing every registry source with a source **kind**.
It also avoids retaining registry strings that could contain credentials.

`lock-summary.json` identifies itself as `zed.nix-lock-summary/v1`. For each
locked package it retains organization, name, version, artifact SHA-256, size,
archive format, and a normalized source kind such as
`immutable-nix-store-input`, `file`, `https`, or `http`. It never retains the
literal source URL. It also records the exact raw-lock digest and the number of
canonical Nix adapter records present in the input lock.

`bridge.json` identifies itself as `zed.nix-fetch-bridge/v1` and explicitly
sets `canonical_adapter_record` to `false`. It is not the canonical
`zed.nix-adapter/v1` provenance/attestation record now defined and
schema-published by `zed-interfaces`. The deterministic exporter and
Nix-to-Zed sealer will emit that shared record. Keeping these formats distinct
prevents a materialization receipt from being mistaken for publication
evidence.

The fetch stage:

- requires `.zpkg.lock` and runs only frozen resolution;
- verifies that frozen installation leaves lock and manifest bytes unchanged;
- uses copy mode, never global-store links;
- disables dependency build hooks;
- isolates `HOME`, XDG directories, and `ZED_PKG_HOME`;
- removes token and password environment variables;
- accepts only proxy variables as impure networking configuration;
- keeps an explicitly declared immutable `registryPath` in the sandbox closure
  and addresses that exact store identity, preserving the `file://` source
  already frozen into `.zpkg.lock` across Linux and Darwin;
- retains digests and redacted inventory rather than raw source-bearing files;
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
{ pkgs }:

let
  # `pkgs` must come from the consumer's locked flake input.
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

Consumers must pin both zed-cli and Nixpkgs in their own `flake.lock`. The lock
committed under `nix/flake.lock` establishes the repository CI baseline and is
kept aligned with the merged `zed-interfaces` Nix intent, provenance, and schema
contract; it does not silently replace the consumer's package-set decision.

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
- a copy of raw source-bearing lock or manifest files;
- the canonical `zed.nix-adapter/v1` publication record;
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

`zed interop nix export` should generate a deterministic package bundle that
calls this bridge or directly consumes exact immutable Zed artifacts with:

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
credential. Version 1 covers no-build archives, prebuilt binaries already
present in immutable artifacts, and fully frozen dependency graphs; native
source-builder inference remains out of scope.

### Tier 2: maintained zed-pkg overlay and binary cache

A project-operated overlay can expose reviewed exports with a cache populated by
CI. Cache publication must bind the Nix output identity to the originating Zed
release, `.zpkg.lock` digest, source commit/tag, artifact digest, canonical
adapter record, and build attestation. The overlay and cache are distribution
channels, not independent resolvers.

### Tier 3: optional upstream Nixpkgs contribution

An upstream contribution is appropriate only when the package meets Nixpkgs
policy for source availability, licensing, maintainership, supported systems,
build reproducibility, update workflow, and review. Automation may prepare a
candidate patch; it must not submit or merge packages blindly.

## Importing Nix packages into zed-pkg

Nix-to-Zed import is deliberately narrower than "accept a Nix expression". A
safe import starts from a fully locked and evaluated output identity.

A normalized import descriptor must record at least:

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
per-output store NAR hashes and sizes
runtime store references
platform constraints
license metadata
source metadata
policy profile
provenance and attestations
```

The importer accepts a descriptor generated from a pinned flake lock or an
explicit immutable revision. It copies or archives only the selected, verified
outputs into a deterministic Zed artifact and records canonical adapter
provenance in registry metadata. Strict portable version 1 rejects outputs with
runtime store references instead of publishing a package that breaks outside
one Nix store.

The following inputs must fail closed:

- mutable branches, tags without immutable revision identity, and channels;
- unlocked flake inputs;
- impure evaluation;
- hidden network fetches whose hashes are not known;
- arbitrary functions requiring caller-provided secrets or host state;
- unsupported dynamic overlays or system-dependent attribute selection;
- output sets whose boundaries or hashes cannot be enumerated;
- non-portable outputs with unresolved runtime store references; and
- platform assumptions that do not match the requested Zed target.

A Nix import is immutable packaging of proven outputs. It does not make the
zed-pkg resolver depend on Nix and it does not require Nix at application
runtime.

## CLI surface

The approved command family keeps planning, export, sealing, and verification
explicit:

```text
zed interop nix plan export --frozen --json
zed interop nix export --frozen --system <system> --nixpkgs-lock <path> --out <dir>
zed interop nix plan import --flake <locked-ref> --attribute <attr> --system <system>
zed interop nix import --plan <canonical-json> --out <artifact>
zed interop nix verify --record <canonical-json>
zed fetch --frozen --output <dir>   # future resolver-only materialization primitive
```

`zed fetch` should populate an explicit output/cache directory without project
links, adapters, build hooks, or source-tree mutation. Once shipped, the Nix FOD
can use it instead of extracting the materialized paths from copy installation.
The lockfile remains the only graph-resolution input in either implementation.
Like the current bridge, a future fetch index must retain digests and normalized
source kinds rather than literal Nix store paths or credential-bearing URLs.

## Platform boundary

The canary exercises Linux and macOS. Windows support should be defined through
WSL or a future native Nix implementation; the bridge must not imply that a
Linux or Darwin store output is portable to native Windows. Imported and
exported metadata must always include the evaluated system and output platform.

## CI contract

`.github/workflows/nix-interop.yml` proves the current boundary by:

1. installing a commit-pinned Nix action and loading Nixpkgs from the committed
   flake lock without updating it;
2. building the exact zed-cli branch;
3. normalizing the Linux runner-built binary's interpreter and RPATH inside Nix;
4. publishing the existing Node fixture to a local registry;
5. adding that registry to the Nix store before resolution so the frozen lock
   points at an immutable URL;
6. generating a real `.zpkg.lock`, recording its exact digest, and uninstalling
   the project materialization;
7. evaluating the standalone flake library with lock updates disabled;
8. bootstrapping the recursive fixed-output hash while addressing the declared
   registry input through the exact store identity recorded in the frozen lock;
9. rebuilding the same graph under two derivation names and comparing SHA-256
   NAR hashes;
10. checking that lock/manifest digests and the source-redacted inventory match
    the original inputs while raw source-bearing files are absent;
11. proving that the FOD has no retained Nix store references;
12. checking the self-identified non-canonical bridge metadata, link policy, and
    disabled dependency build hook;
13. building and checking a normal offline consumer derivation;
14. rejecting an incorrect Nix output hash; and
15. rejecting a lockfile whose Zed artifact digest was tampered with.

This canary is intentionally based on the same copy-install fixture used for OCI
ownership tests, so Nix and container boundaries enforce one materialization
contract instead of drifting into separate implementations.
# DEN-1411 Nix interoperability proof of concept

The normative design is `zed-docs/docs/23-nix-zed-interop.md`. This repository
contains the first executable oracle for that RFC while the typed Rust CLI and
shared schema are staged.

The implementation deliberately does **not** add Nix to `NativeRegistry` and
does not make ordinary Zed commands depend on Nix. Each direction has one
resolution authority and crosses an immutable artifact boundary.

## Zed artifact → standalone Nix flake

```sh
python tools/zed_nix_bridge.py zed-to-nix \
  --manifest .zpkg.toml \
  --metadata version.json \
  --nixpkgs-lock policy/nixpkgs.lock \
  --system x86_64-linux \
  --out-dir dist/nix

python tools/zed_nix_bridge.py verify --directory dist/nix
nix flake check --no-update-lock-file dist/nix
nix build --no-update-lock-file dist/nix
```

The current adapter supports dependency-free, already-published data archives
and prebuilt executable artifacts. It validates package identity, version,
HTTPS artifact URL, SHA-256, VCS tag/commit, explicit systems, and an exact
Nixpkgs lock. It rejects arbitrary `[build]` commands and non-empty dependency
graphs rather than approximating Zed's resolver or copying a shell command into
a derivation.

The generated bundle contains:

- `flake.nix` and the supplied immutable `flake.lock`;
- `nix/package.nix` using a fixed artifact URL and SRI hash;
- `zed-nix-adapter.json` with canonical input and generated-file evidence; and
- a README describing the installed layout.

Nix installs the exact package tree under
`$out/share/zed-pkg/<org>/<name>`. Validated `[bin]` entries are exposed through
`$out/bin`. The derivation records exact Zed identity and VCS provenance in
`passthru.zed`; the installed output never requires Zed.

## Realized Nix output → sealed Zed artifact

```sh
python tools/zed_nix_bridge.py nix-to-zed \
  --store-path /nix/store/...-portable-output \
  --path-info path-info.json \
  --derivation-json derivation.json \
  --flake-lock flake.lock \
  --locked-ref github:acme/tool/<immutable-commit> \
  --attribute packages.x86_64-linux.portable \
  --system x86_64-linux \
  --output out \
  --as-package acme/tool@1.2.3 \
  --bin tool=bin/tool \
  --repository https://github.com/acme/tool \
  --source-revision <immutable-commit> \
  --source-available \
  --license MIT \
  --description 'Portable tool' \
  --nix-version "$(nix --version)" \
  --out-dir dist/zed

python tools/zed_nix_bridge.py verify --directory dist/zed
```

This command does not evaluate Nix. It consumes evidence for one explicitly
selected, already-realized output and seals its bytes into a deterministic Zed
`tar.gz`. Strict version 1 requires:

- immutable locked source provenance and exact flake-lock bytes;
- explicit attribute, system, and output name;
- `nix derivation show` and `nix path-info --json` evidence;
- NAR hash/size and an empty external reference set;
- no concrete `/nix/store/<hash>-...` strings in regular files;
- no absolute, escaping, or store-pointing symlinks;
- only regular files, directories, and safe symlinks;
- explicit package identity, source revision/availability, repository, license,
  description, and exported binaries.

The archive normalizes path order, ownership, timestamps, and modes. It embeds
`.zpkg.toml` plus `zed-nix-adapter.json`; `bridge.json` is the external sidecar
that records the resulting Zed artifact SHA-256 and size. The sealed package
contains no Nix launcher and does not require Nix at install or runtime.

Outputs with runtime references are rejected with the exact referenced paths.
The adapter does not rewrite binaries, hide a closure in a wrapper, or pretend
one store output is portable when it is not. Such packages should remain in
Nix until a separately reviewed typed closure-bundling adapter exists.

## Verification and tamper behavior

`verify` handles both generated standalone flakes and sealed Zed artifacts. It
fails on unknown schema versions, missing/generated-file hash drift, symlinked
control files, unsafe archive paths, duplicate archive members, special file
types, embedded store references, unresolved Nix references, altered embedded
adapter metadata, artifact hash/size drift, or a runtime-Nix dependency claim.

The Python unit suite covers deterministic replay, metadata/lock drift,
unsupported build/dependency graphs, mutable references, output overwrite and
symlink attacks, closure/reference leakage, unsafe symlinks, missing binaries,
artifact tampering, and sidecar/embedded-record disagreement.

## Next slices

1. Land the versioned adapter types and JSON Schema in `zed-interfaces`.
2. Add `zed interop nix plan export|import`, `export`, `import`, `verify`, and
   later `publish` in the Rust CLI, consuming the same schema.
3. Export frozen multi-package `.zpkg.lock` graphs without Nix-side resolution.
4. Persist adapter digests in `.zpkg.lock` and registry publication metadata.
5. Add Linux/macOS canaries for data, prebuilt Rust, Node/TypeScript,
   multi-output selection, impurity rejection, offline replay, and tampering.
6. Build a reviewed Zed overlay plus signed Cachix/Attic policy. Upstream
   Nixpkgs remains a human-reviewed destination, never an automatic side effect.
