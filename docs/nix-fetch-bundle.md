# Resolver-only Zed artifact bundles in Nix

The Nix library exposes two intentionally different fixed-output boundaries:

| Helper | Zed operation | Output shape | Intended consumer |
| --- | --- | --- | --- |
| `fetchZedDeps` | frozen copy installation | project adapter/materialization tree | `mkZedPackage` and ordinary project builds |
| `fetchZedArtifacts` | `zed fetch --frozen` | source-redacted `zed.fetch/v1` artifact bundle | external builders, caches, exporters, and future offline materializers |

`fetchZedArtifacts` does not replace `fetchZedDeps`. A resolver-only bundle has
no `node_modules`, `zed_modules`, native adapter files, workspace links, project
references, or dependency build-hook output. Until an explicit offline
materializer is reviewed, Nix callers that need an installation-shaped tree
should continue using `fetchZedDeps`.

## Example

```nix
{ pkgs }:

let
  zedNix = import ./vendor/zed-cli/nix { inherit pkgs; };

  artifacts = zedNix.fetchZedArtifacts {
    pname = "acme-service-zed-artifacts";
    version = "1";
    src = ./.;
    zed = pkgs.zed-pkg;
    hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
  };
in
pkgs.runCommand "inspect-acme-zed-artifacts" { } ''
  test -f ${artifacts}/metadata/index.json
  test -f ${artifacts}/metadata/lock.sha256
  mkdir -p "$out"
  cp -a ${artifacts}/metadata/. "$out/"
''
```

Start with `pkgs.lib.fakeHash` or the placeholder above. The first build fails
and reports the recursive SHA-256 NAR hash. Record that value and rebuild.

For a lock containing an immutable file registry already registered with Nix,
pass the exact store identity:

```nix
artifacts = zedNix.fetchZedArtifacts {
  src = ./.;
  zed = pkgs.zed-pkg;
  registryPath = builtins.storePath
    "/nix/store/…-zed-registry";
  hash = "sha256-…";
};
```

The `registryPath` is a derivation input so the sandbox may read it. The output
index retains only `source_kind = "immutable-nix-store-input"`; it does not
retain the store path or raw registry URL.

## Fixed-output execution

The helper:

1. copies only the requested lock into an isolated temporary project;
2. isolates `HOME`, XDG directories, and `ZED_PKG_HOME`;
3. removes registry tokens, Supabase keys, auth passwords, and ambient fetch
   flags from the environment;
4. creates a writable temporary output parent;
5. runs `zed fetch --frozen --output <temporary-parent>/bundle`;
6. verifies the `zed.fetch/v1` schema, exact lock digest, canonical package
   ordering, content-addressed payload paths, source redaction, and payload
   presence;
7. rejects symlinks and project-install state; and
8. copies the verified bundle into `$out` for Nix recursive hashing.

Nix provides the final recursive output identity. zed-pkg remains the only graph
resolver and artifact verifier.

## Output

```text
$out/
├── packages/
│   └── <artifact-sha256>/
│       └── pkg/...
└── metadata/
    ├── index.json
    ├── lock.sha256
    └── zed-version.txt
```

The FOD must have no runtime references to other Nix store objects. CI checks
that with `nix-store --query --references` and rejects any raw `/nix/store/`
string in portable metadata.

## Security and reproducibility limits

- `.zpkg.lock` is the sole graph authority.
- Private registry credentials are not inherited. A separate reviewed secret
  delivery design is required.
- Dependency build hooks never run.
- Literal registry sources, raw locks, manifests, caller paths, and temporary
  paths are not retained in the output.
- An incorrect Nix output hash fails closed.
- A changed registry artifact digest, size, format, identity, or VCS provenance
  fails before output publication.
- The same frozen graph is replayed under different derivation names in Linux
  and macOS CI and must produce the same SHA-256 NAR hash.
- Dependency-free locks are valid deterministic bundles.

An eventual `zed install --offline --cache <bundle>` command can consume this
format only after its project-mutation, adapter, and build-hook boundaries are
specified independently.
