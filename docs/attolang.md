# AttoLang (formerly Oreslang): Zed package imports

Zed is the **package resolver, immutable artifact store, and lockfile owner**.
AttoLang remains the **compiler and import resolver**. This first integration
does not introduce another resolver, execute dependency code, or modify the
AttoLang compiler. It projects installed Zed package roots into the current
AttoLang import-search environment.

## Quickstart

Keep both manifests at the same project root:

- .atto.pkg.toml: canonical AttoLang project-root and import semantics;
  legacy .ores.pkg.toml is supported only when the canonical file is absent.
  Never publish both marker files in one package root.
- .zpkg.toml and .zpkg.lock: package versions, artifacts, VCS provenance and
  deterministic restoration controlled by Zed.

From your AttoLang project root:

    zed init --org acme
    zed add acme/actor-math@^1
    zed install --frozen
    export ATTO_PATHS="$(zed attolang paths)"

Use your normal AttoLang compiler invocation after setting ATTO_PATHS.
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

Publish an AttoLang source package with exactly one compiler-owned native
marker: canonical .atto.pkg.toml or legacy .ores.pkg.toml. The bridge exports
installed Zed package roots only when one regular-file marker is present.
A package merely tagged attolang or oreslang in the installed index is
**not** enough to authorize compiler imports: a missing source manifest,
ambiguous twin markers, symlink marker or directory marker causes rejection.

Canonical language = "attolang" and the legacy oreslang alias are merged in
[zed-interfaces #183](https://github.com/zed-pkg/zed-interfaces/pull/183).
Use language = "attolang" only with a CLI whose **pinned** zed-interfaces
revision includes that contract. Older CLIs must retain their compatible
universal label during migration, not silently accept an unknown token.

Source packages continue to be installed under an org/name slot:

    zed_modules/acme/actor-math

Zed materializes the slot in symlink or copy mode. AttoLang sees the slot
through ATTO_PATHS (or legacy ORES_PATHS) and does not need to know the global artifact-store path.

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

1. **Already merged upstream:** canonical Language::Attolang and
   Ecosystem::Attolang, with oreslang as legacy input, in zed-interfaces #183.
   Next, update and test downstream exact dependency pins plus generated SDK
   schemas, and migrate native project markers to .atto.pkg.toml.
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
or completed cross-platform guest CI is claimed by this document.
