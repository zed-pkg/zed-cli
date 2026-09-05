# `generated/` — frozen artifacts (read-only)

This tree is **generated**. Do not hand-edit adapters here.

Typical producers:

- [`flags-2-env`](https://github.com/flags-2-env/flags-2-env-cli) (`f2e generate`)
- [`api-docs` / `ridl`](https://github.com/oresoftware/api-docs)
- interface adapters from `schema/tables.json` (`node src/generate.mjs`)

## Read-only on disk

After generate, artifact files are `chmod a-w` (0444). Directories stay
writable so the generator can add files.

**Git does not store the Unix write bit** — only the executable bit
(100644 vs 100755). After `git clone` / `git checkout`, files come back
writable. Restore the policy with the generator (`f2e generate`,
`ridl generate`, `node src/generate.mjs`) or:

```sh
scripts/freeze-generated.sh
```

Do not `chmod u+w` and then commit a hand-edit. Change the source catalog
(`.cli-flags.toml`, route map, `schema/tables.json`) and regenerate.

## JSON Schema (the contract)

If `json-schema/` is present, those documents are JSON Schema 2020-12.
Compile-time types are generated from that catalog. Runtime
`check_os_env` / `checkOsEnv` / `validate()` must pass on real payloads,
not only on types that compile. Unit tests should feed **valid** and
**invalid** instances (missing required keys, wrong types, extra properties).

```sh
f2e check-contract --config .cli-flags.toml --json env.fixture.json
```

## Gitignored trees

If this folder is listed in `.gitignore`, artifacts stay local. Keep this
README tracked with:

```
generated/*
!generated/README.md
```

(Do not ignore the directory node itself as `generated/` — that prevents
the `!README.md` exception from working.)
