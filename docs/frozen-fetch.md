# Resolver-only frozen fetch

`zed fetch` exports the dependency graph already pinned by `.zpkg.lock` without
installing that graph into the project:

```sh
zed fetch --frozen --output /tmp/my-project-zed-deps
```

Version 1 is deliberately frozen-only. It never performs semver resolution,
selects a latest version, rewrites `.zpkg.lock`, runs a package build hook, or
creates ecosystem adapters.

## Output contract

The output is a new directory published through an atomic rename:

```text
my-project-zed-deps/
├── packages/
│   └── <artifact-sha256>/
│       └── pkg/
│           └── ... verified package payload ...
└── metadata/
    ├── index.json
    ├── lock.sha256
    └── zed-version.txt
```

`metadata/index.json` uses schema `zed.fetch/v1`. Packages are ordered by
identity and include their exact version, archive SHA-256, size, format, VCS
provenance, normalized source kind, and relative payload path.

The index intentionally does **not** retain registry URLs. A lock may point at a
private mirror, signed endpoint, local path, or immutable Nix store object; that
literal can disclose credentials or make a supposedly portable bundle retain a
host-specific path. The source is classified only as `https`, `http`, `file`,
or `immutable-nix-store-input`.

## Trust boundary

For every locked package, fetch verifies all of the following before publishing
the final output:

- package organization, name, and exact version;
- archive SHA-256 and reported byte size;
- archive format;
- VCS tag and optional commit;
- safe package identity and digest syntax;
- supported, credential-free registry source syntax; and
- hardened archive extraction with traversal, symlink, special-file,
  decompression-size, and entry-count rejection.

Downloads and extraction use a temporary isolated Zed store beside the output.
The command does not write `ZED_PKG_HOME`, `refs.json`, `zed_modules/`,
`node_modules/`, `.zed/`, build-cache output, or project transaction state.

The output must not exist and must be outside the project tree. Paths containing
`..` are rejected. Temporary bundle and store directories are removed after
success and through error cleanup; the final directory appears only after all
packages and metadata have been verified and written.

## Nix use

The Nix fixed-output bridge can migrate from frozen copy installation to this
primitive:

```nix
installPhase = ''
  zed fetch --frozen --output "$out"
'';
```

Nix remains the recursive output verifier, while `.zpkg.lock` and zed-pkg remain
the only graph resolver and artifact verifier. The generated fetch bundle is
not a project install and does not contain native ecosystem wiring.

## Deliberate exclusions

Private registry authentication needs an explicit secret-delivery contract.
Credentials, query strings, and URL fragments embedded directly in a frozen
lock source fail closed. Offline project materialization from a fetch bundle is
a separate operation and should eventually use an explicit command such as:

```text
zed install --offline --cache <fetch-bundle>
```

That future installer must consume the bundle without turning `zed fetch` into
a project-mutating command.