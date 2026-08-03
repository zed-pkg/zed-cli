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
