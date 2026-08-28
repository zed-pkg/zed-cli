# Resolver-only frozen fetch

`zed fetch` exports the dependency graph already pinned by `.zpkg.lock` without
installing that graph into the project:

```sh
mkdir -p /tmp/zed-fetch-outputs
zed fetch --frozen --output /tmp/zed-fetch-outputs/my-project-zed-deps
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

The shared lock parser owns structural integrity and therefore fails first for
malformed versions, duplicate package identities, invalid package names,
non-canonical digests, missing immutable VCS revisions, and inconsistent Nix
adapter evidence. Fetch-specific verification starts only after that parser
accepts the complete lock. Tests that need malformed lock bytes serialize those
fixtures through an explicitly test-only unchecked helper; production writers
continue to validate before emitting a lock.

Error reporting preserves that layered boundary. The top-level context identifies
which phase failed, while the complete error chain retains the actionable shared
parser or fetch-verifier cause. Credential-bearing source literals, tokens,
query values, and rejected secret material must never be echoed by either layer.

Downloads and extraction use a temporary isolated Zed store beside the output.
The command does not write `ZED_PKG_HOME`, `refs.json`, `zed_modules/`,
`node_modules/`, `.zed/`, build-cache output, or project transaction state.

The output must not exist and must be outside the project tree. Paths containing
`..` are rejected. Its parent must already exist and be a directory. That parent
is canonicalized before any temporary or final state is created, so a symlink
cannot redirect an apparently external output back into the project. Temporary
bundle and store directories are removed after success and through error
cleanup; the final directory appears only after all packages and metadata have
been verified and written.

`file://` lock sources must resolve to local absolute paths. User information,
passwords, query strings, fragments, and non-local authorities fail closed, and
diagnostics do not echo the rejected source literal. The only accepted file
authority is the URL-parser domain `localhost` (or no host at all); loopback
IP literals are rejected before `Url::to_file_path()` so Windows cannot
reinterpret them as UNC.

## Nix use

A Nix fixed-output derivation should ask `zed fetch` to publish into an existing
writable temporary parent and then copy the verified bundle into `$out`:

```nix
installPhase = ''
  set -euo pipefail
  bundle_parent="$TMPDIR/zed-fetch-output"
  mkdir -p "$bundle_parent"

  zed fetch \
    --frozen \
    --output "$bundle_parent/bundle"

  mkdir -p "$out"
  cp -a "$bundle_parent/bundle/." "$out/"
'';
```

Writing directly to `$out` is not the intended atomic-fetch boundary: the Nix
store path and its parent are controlled by Nix, while `zed fetch` needs a
writable pre-existing sibling directory for staging and same-filesystem rename.
Nix still verifies the recursively hashed final `$out`. `.zpkg.lock` and
zed-pkg remain the only graph resolver and artifact verifier. The generated
fetch bundle is not a project install and does not contain native ecosystem
wiring.

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
