# Strict Nix → Zed sealing

This is the executable first slice of DEN-1419 and the merged Nix
interoperability RFC in `zed-docs/docs/23-nix-zed-interop.md`.

Nix remains the only resolution authority. The bridge accepts one explicitly
selected, already-realized output and refuses to evaluate arbitrary expressions
or infer a package from a flake. The resulting `tar.gz` is an ordinary Zed
artifact: installing or running it does not require Nix.

## Seal one explicit output

```sh
output=$(nix build \
  --no-write-lock-file \
  --no-update-lock-file \
  .#portable \
  --no-link \
  --print-out-paths)

nix path-info --json "$output" > path-info.json
derivation=$(nix-store --query --deriver "$output")
nix derivation show "$derivation" > derivation.json
system=$(nix eval --raw --impure --expr builtins.currentSystem)

python3 tools/nix_to_zed.py seal \
  --store-path "$output" \
  --path-info path-info.json \
  --derivation-json derivation.json \
  --flake-lock flake.lock \
  --locked-ref github:acme/tool/<immutable-commit> \
  --attribute "packages.$system.portable" \
  --system "$system" \
  --output out \
  --as-package acme/tool@1.2.3 \
  --bin tool=bin/tool \
  --repository https://github.com/acme/tool \
  --source-revision <immutable-commit> \
  --license MIT \
  --description 'Portable tool' \
  --nix-version "$(nix --version)" \
  --out-dir dist/zed

python3 tools/nix_to_zed.py verify --directory dist/zed
```

## Outputs

The output directory contains:

- `<name>-<version>.tar.gz`: deterministic Zed package bytes;
- `zed-nix-adapter.json`: canonical compact `zed.nix-adapter/v1` record matching
  the types and generated schema in `zed-interfaces`;
- `bridge.json`: hashes binding the artifact, adapter, manifest, and embedded
  runtime-safe provenance projection.

The archive contains the exact selected output tree, a generated `.zpkg.toml`,
`README.zed-nix.md`, and `zed-nix-runtime.json`. The runtime projection contains
a SHA-256 of the selected store path, never the exact `/nix/store/...` string.
The exact diagnostic store path remains only in the external canonical adapter.

## Strict-v1 acceptance boundary

Sealing fails unless all of the following hold:

- the source selector contains an immutable revision or NAR-hash proof;
- exact `flake.lock`, derivation JSON, attribute, system, output, NAR hash/size,
  Nix version, and store-info JSON version are supplied;
- the realized output has no external Nix store references;
- regular files contain no concrete `/nix/store/<hash>-...` strings;
- symlinks are relative, remain inside the selected output, and do not point to
  the Nix store;
- all entries are regular files, directories, or safe symlinks;
- every exported binary exists inside the output and is executable;
- the package identity, repository, source revision, license, and description
  are explicit;
- existing or symlinked output control files are not overwritten unless the
  caller explicitly uses `--force` for regular files.

The canonical policy record is fail-closed: pure evaluation, IFD disabled,
sandbox required, builder network disabled, clean source, and publishable
output. Contract v1 does not bundle a Nix closure, rewrite ELF/Mach-O binaries,
ship a launcher that calls Nix, or hide store dependencies in wrappers.

## Publication

This PR only creates and verifies package bytes plus canonical provenance.
Registry upload remains a separately authorized Zed publish action. A future
`zed interop nix import|verify|publish` Rust command should consume the same
canonical record and preserve confirmation, signing, and registry policy.
