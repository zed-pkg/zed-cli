# AttoLang (formerly Oreslang): Zed package imports

Zed is the **package resolver, immutable artifact store, and lockfile owner**.
AttoLang remains the **compiler and import resolver**. This first integration
does not introduce another resolver, execute dependency code, or modify the
AttoLang compiler. It projects installed Zed package roots into the current
AttoLang import-search environment.

## Quickstart

Keep both manifests at the same project root:

- .ores.pkg.toml: AttoLang project-root and import semantics.
- .zpkg.toml and .zpkg.lock: package versions, artifacts, VCS provenance and
  deterministic restoration controlled by Zed.

From your AttoLang project root:

    zed init --org acme
    zed add acme/actor-math@^1
    zed install --frozen
    export ATTO_PATHS="$(zed attolang paths)"

Use your normal AttoLang compiler invocation after setting ORES_PATHS.
The AttoLang plus-prefixed import syntax continues to resolve against the
project root; Zed contributes *dependency* import roots, not a new definition
of plus imports.

The initial bridge also accepts the legacy command spelling:

    zed oreslang paths

For tools that cannot safely consume colon-separated paths, use:

    zed attolang paths --json

The JSON object contains env (ATTO_PATHS), legacy_env (ORES_PATHS), paths
(an ordered string array), and value (joined with the host OS path-list
separator; ':' on POSIX and ';' on Windows). An explicit empty ATTO_PATHS
prevents ambient ORES_PATHS fallback in compilers honoring the current
AttoLang import-resolution contract. The legacy zed oreslang command spelling
is still accepted, but returns canonical ATTO_PATHS metadata.

## Package authors

Publish an AttoLang source package with its native .ores.pkg.toml included
in the archive. The current bridge selects installed Zed packages with that
file at their installed root. It also recognizes an indexed package whose
language is already tagged attolang or oreslang for forward compatibility.
**Do not set language = "attolang" in .zpkg.toml yet**: the shared
zed-interfaces manifest language enumeration has not been extended, and the
existing validator rejects that token.

Source packages continue to be installed under an org/name slot:

    zed_modules/acme/actor-math

Zed materializes the slot in symlink or copy mode. AttoLang sees the slot
through ORES_PATHS and does not need to know the global artifact-store path.

## Determinism and security

- Read-only: the command reads .zed/paths.json and the installed directory
  slots, but does not fetch, resolve, run scripts, rewrite manifests or locks,
  or modify environment variables in the calling shell.
- Deterministic: one deduplicated, lexically sorted path per AttoLang package.
- Cross-directory: paths are relative to the invoking working directory,
  including when called from below the project root.
- Path safety: index entries must be under the declared module slot with
  matching org/name and cannot contain traversal, absolute paths, colon,
  newlines, carriage returns, or NUL.
- Other language packages are excluded unless they carry the native AttoLang
  marker. No automatic execution of package install hooks is enabled.
- This projection **does not independently prove artifact integrity or lockfile freshness**.
  Package verification and frozen lockfile enforcement happen during
  zed install --frozen, which must complete successfully before export.
- On Windows, ATTO_PATHS now emits the host-native ';' path-list separator.
  JSON remains the transport-neutral option. Each compiler must independently
  verify its ATTO_PATHS parsing and installed-source provenance.

A missing or stale installed index fails with an actionable diagnostic;
rerun zed install --frozen. Do not hand-edit .zed/paths.json.

## Next native-language contract milestone

The bridge is deliberately incremental. Full first-class package-management
support requires a separate **zed-interfaces-first** change:

1. Add canonical Language::Attolang and Ecosystem::Attolang, with oreslang
   accepted as a legacy input alias, .ores.pkg.toml as an ecosystem marker,
   and canonical lowercase attolang serialization. Add Rust/TS/Dart round
   trips and parity cases before regenerating JSON Schema outputs.
2. Teach zed-cli target slicing, manifestless project discovery, and package
   routing to recognize attolang without treating Oreslang and AttoLang as
   different published package identities. Retain existing source imports
   and exact lockfile pins.
3. Confirm each AttoLang compiler's ATTO_PATHS > ORES_PATHS >
   ORESLANG_PATH presence-based precedence, +/ project-root independence,
   native path-list parsing, and module-name collision rules.
   Add a compiler-level round-trip in zed-e2e instead of assuming the CLI's
   path projection itself proves successful imports.
4. Add backend-specific build receipts and cache keys for Java/GraalVM, LLVM,
   Wasm, Forge, and .NET. The same locked source artifact may feed multiple
   compiled outputs; caches must never mix their ABIs or permissions.
5. Add hermetic publish -> registry -> frozen install -> compile -> execute
   checks, including yanked version behavior, offline cached install,
   mismatched target rejection, poisoned archives, symlink boundaries,
   concurrent installs, dependency conflict diagnostics, and key rotation.

No runtime parity, compiler integration, native package publication,
Windows path semantics, or completed CI execution is claimed by this document.
