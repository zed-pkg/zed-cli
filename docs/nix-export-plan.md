# Deterministic frozen Zed → Nix export planning

The first execution-independent Zed → Nix operation is:

```sh
zed interop nix plan export --frozen --json
```

For a polyglot package, select the exact published slice:

```sh
zed interop nix plan export \
  --frozen \
  --target nodejs \
  --json
```

The command is read-only. It validates author intent, hashes the exact source
inputs, builds the normal deterministic Zed artifact in a temporary directory,
and emits a stable `zed.nix-export-plan/v1` document. It does not evaluate Nix,
contact a registry, read saved credentials, write the project, update a lock,
publish an overlay, or create a binary-cache entry.

## Version 1 scope

The first planner accepts:

- dependency-free data/archive packages;
- dependency-free prebuilt executable packages whose declared `[bin]` files
  already exist in the selected source and survive the publish exclusion set;
- single-language `[publish.nix]` intent; and
- explicit polyglot `[targets.<target>.nix]` intent.

It fails closed for:

- a missing `--frozen` opt-in or missing `.zpkg.lock`;
- dependencies in the manifest or frozen lock;
- build hooks, build dependencies, override build hooks, and workspace roots;
- a missing or ambiguous Nix route;
- an unknown polyglot target;
- a missing prebuilt executable;
- a declared executable excluded from the immutable artifact; and
- invalid systems, outputs, attributes, bin paths, or other manifest fields.

Locked dependency-graph assembly and source-builder execution remain separate
reviewed slices. The planner never silently falls back to native package-manager
resolution.

## Required manifest intent

Single-language package:

```toml
[publish.nix]
mode = "artifact"
attribute = "http-kit"
systems = ["x86_64-linux", "aarch64-linux"]
outputs = ["out"]
```

Polyglot target:

```toml
[targets.nodejs]
dir = "clients/node"
adapter = "node"

[targets.nodejs.nix]
mode = "artifact"
attribute = "clients-node"
systems = ["x86_64-linux", "aarch64-linux"]
outputs = ["out"]
```

`mode` defaults to `artifact`. Systems and outputs must be explicit. Attributes
are validated and collision-checked by the shared `zed-interfaces` contract.
The planner sorts non-semantic systems and outputs before serialization.

## Plan document

A plan has this shape:

```json
{
  "schema": "zed.nix-export-plan/v1",
  "package": {
    "org": "acme",
    "name": "http-kit",
    "version": "1.2.3"
  },
  "package_class": "data",
  "intent": {
    "mode": "artifact",
    "attribute": "http-kit",
    "systems": ["aarch64-linux", "x86_64-linux"],
    "outputs": ["out"]
  },
  "source": {
    "file_name": "acme-http-kit-1.2.3.tar.gz",
    "artifact": {
      "format": "tar.gz",
      "sha256": "…",
      "size": 1234
    },
    "manifest_sha256": "…",
    "lock_sha256": "…"
  },
  "bins": {},
  "dependencies": [],
  "policy": {
    "profile": "strict-v1",
    "pure_evaluation": true,
    "import_from_derivation": false,
    "sandbox_required": true,
    "builder_network": "disabled",
    "dirty_source": false,
    "publishable": true
  }
}
```

The exact `.zpkg.toml` and `.zpkg.lock` byte digests are retained. The plan does
not retain their absolute paths. The deterministic packed-artifact digest binds
the publish exclusion rules, executable modes, derived target manifest, and all
other artifact bytes.

The document contains no registry URL, token, Supabase key, auth URL, Zed home,
workspace path, temporary directory, username, hostname, timestamp, cache key,
or mutable version request. Global CLI settings are accepted for consistency
but are intentionally never converted into runtime configuration by this
command.

## Determinism contract

Given identical package files, manifest bytes, lock bytes, selected target, and
shared interface version, clean directories at different absolute paths must
produce identical compact JSON bytes.

Changing a comment or formatting in the exact manifest changes
`manifest_sha256` and generally changes the packed artifact because
`.zpkg.toml` is part of the Zed artifact. Changing lock formatting changes
`lock_sha256`. Neither change is silently normalized away.

## Relationship to export execution

The next command consumes a reviewed plan and an approved immutable Nixpkgs lock
template:

```text
zed interop nix export \
  --frozen \
  --system <system> \
  --nixpkgs-lock <path> \
  --out <new-directory>
```

That later slice generates a standalone flake bundle containing the immutable
Zed artifact, package expression, pinned `flake.lock`, canonical adapter record,
and operator README. Planning does not create those files and never publishes
them automatically.
