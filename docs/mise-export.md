# Deterministic mise export

`zed env export mise` projects a schema-v2 Zed environment plan into a
project-local mise configuration without invoking mise, loading global state,
or executing project code.

```sh
# Print deterministic TOML without writing files.
zed env export mise --plan zed-env.toml

# Verify that the checked-in manager view is current.
zed env export mise --plan zed-env.toml --output .mise.toml --check

# Create or update a Zed-owned manager view transactionally.
zed env export mise --plan zed-env.toml --output .mise.toml --write
```

`--check` and `--write` are mutually exclusive. Without either flag, the
command prints the deterministic TOML document to standard output.

## Supported projection

The first write-capable slice exports only state with an exact, tested mise
representation:

- ordered single- and multi-version tools;
- backend-qualified tool keys and core-tool aliases;
- `version`, portable project-relative `path`, `prefix`, and `ref` selectors;
- typed backend options and tool OS constraints;
- scalar environment values;
- typed project and task variables;
- task descriptions, aliases, ordered command arrays, dependencies,
  post-dependencies, readiness dependencies, environment, variables,
  task-local tools, directories, sources, outputs, one shell string, usage,
  confirmation, cache, timeout, and output flags; and
- lockfile platform settings.

The exporter preserves multi-version and command order. Set-like platform
lists are sorted and deduplicated by the normalized environment contract.

## Fail-closed boundaries

The command rejects fields rather than silently omitting them when no certified
mapping exists. Current explicit boundaries include:

- activation hooks other than `activation = "none"`;
- environment-plan system packages;
- imported manager provenance and lock identity;
- extension maps;
- resolved versions, immutable sources, and artifact checksums that belong in
  `mise.lock` rather than `mise.toml`;
- complex top-level or task environment tables/arrays, which mise may interpret
  as directives rather than literal values;
- grouped and structured task invocations until the current mise run-entry wire
  form is certified; and
- shell program-plus-argument vectors, because the current mise task field is
  one shell string.

Diagnostics include the exact environment-plan path that cannot be represented.
Complete current `mise.lock` export is tracked independently so config export
cannot accidentally erase provenance.

## Secrets

The plan contract does not yet expose a portable secret-reference type. Export
therefore rejects literal environment, variable, and tool-option names that
look credential-bearing at any nesting depth, including password, secret, token,
private/access key, API key, credential, and authorization names. The generated file, sidecar,
logs, and diagnostics never print rejected values.

## Ownership and conflicts

Write mode records deterministic ownership in:

```text
.zed/mise-export-state.json
```

The sidecar contains only schema version, project-relative plan/output paths,
and SHA-256 identities. It contains no timestamps, random identifiers,
absolute paths, credentials, or environment values.

The exporter follows these rules:

1. A missing output can be created.
2. An existing file whose bytes already equal the deterministic projection can
   be adopted safely.
3. A differing existing file with no matching Zed ownership record is treated
   as hand-authored and is never overwritten.
4. A Zed-owned file whose current digest differs from the recorded digest is a
   user edit and is never overwritten.
5. A Zed-owned unchanged file may be replaced only by the same project-relative
   plan recorded in the sidecar.

Output and state changes share `ProjectTransaction`, including crash recovery
and rollback. Plan, output, state, and staging identities are compared with
portable ASCII case-folding so a Linux-generated ownership file cannot become
ambiguous on Windows or macOS. Project, output, and state paths reject absolute/home/drive/UNC,
parent traversal, and symlink escape.

## Determinism

The plan identity is:

```text
SHA256("zed-pkg:mise-export-plan:v1\0" || canonical-environment-plan-json)
```

The output identity is SHA-256 over the exact generated TOML bytes. Print,
check, and write modes all render the normalized semantic plan, so set-like
presentation order cannot diverge under one plan digest. Repeated generation of the
same normalized plan is byte-identical across supported platforms.

## Next gates

- Bind the complete current `mise.lock` contract to export and verification.
- Translate portable manager identities into native `EnvironmentLock`.
- Certify structured task run entries and sandbox capabilities.
- Add conflict-aware import → export → import semantic identity tests on Linux,
  macOS, and Windows.

Tracking: DEN-1462, DEN-1461, DEN-1481.
