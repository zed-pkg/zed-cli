<!-- generated-policy: frozen -->

# `generated/` — frozen artifacts (read-only)

Do **not** hand-edit files in this directory. They are produced by tooling such as:

- [`flags-2-env`](https://github.com/flags-2-env/flags-2-env-cli) (`f2e generate`; typical Dart path: `generated/dart/env.dart`)
- [`api-docs` / `ridl`](https://github.com/oresoftware/api-docs)
- JSON Schema / OpenAPI / route-map generators, including `schema/tables.json` (`node src/generate.mjs`)

## Disk permissions

After generation, artifact files are frozen with `chmod a-w` (0444). Directories
and this `README.md` stay writable so generators can add or replace files.

**Git does not persist the Unix write bit** — only the executable bit
(100644 vs 100755). A fresh clone is writable until you re-freeze with the
generator (`f2e generate`, `ridl generate`, `node src/generate.mjs`) or:

```sh
scripts/freeze-generated.sh
```

The script walks every `generated/` tree whose README is marked frozen (this
file's `generated-policy: frozen` marker). Equivalent one-liner:

```sh
find generated -type f ! -name 'README.md' ! -name 'readme.md' -exec chmod a-w {} +
```

Do not `chmod u+w` and then commit a hand-edit. Change the **primary source**
(`.cli-flags.toml`, route map, OpenAPI, `schema/*.schema.json`, `schema/tables.json`)
and regenerate. Preferred generators thaw, write, then `chmod a-w` themselves.

## JSON Schema (the contract)

JSON Schema is a **cross-check**, not always the primary generator input. If
`json-schema/` is present, those documents are JSON Schema 2020-12. Compile-time
types are generated from that catalog. Runtime `check_os_env` / `checkOsEnv` /
`validate()` must pass on real payloads, not only on types that compile.

Unit tests should feed **valid** and **invalid** instances (missing required
keys, wrong types, extra properties) and compare schema keys to
`.cli-flags.toml` env names or route-map keys when those exist.

```sh
f2e check-contract --config .cli-flags.toml --json env.fixture.json
```

## Gitignored trees

If `generated/` is in `.gitignore`, generated artifacts stay off VCS. Still commit
this `README.md` (`git add -f generated/README.md` or a `.gitignore` exception) so
the freeze policy is visible. Example exception:

```
generated/**
!generated/README.md
```

(Do not ignore the directory node itself as `generated/` — that prevents
the `!README.md` exception from working.)
