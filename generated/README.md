<!-- generated-policy: frozen -->

# Generated files — read-only

Do **not** hand-edit files in this directory. They are produced by tooling such as:

- https://github.com/flags-2-env/flags-2-env (typical Dart path: `generated/dart/env.dart`)
- https://github.com/oresoftware/api-docs
- JSON Schema / OpenAPI / route-map generators in this repository

## Disk permissions

After generation, files here are frozen with `chmod a-w` (not writable). Directories
and this `README.md` stay writable so generators can replace files.

Git does **not** persist the write bit (only the executable bit). A fresh clone is
writable until you re-freeze:

```sh
python3 scripts/check-generated-contract.py --freeze --require-readonly
```

To regenerate, change the **primary source** (`.cli-flags.toml`, route map, OpenAPI,
`schema/*.schema.json`, …) and re-run the generator. Preferred generators thaw,
write, then `chmod a-w` themselves.

## Runtime contract (not just compile-time)

JSON Schema is a **cross-check**, not always the primary generator input. The checker
validates fixtures/examples against Draft 2020-12 at runtime (valid must pass,
invalid must fail) and compares schema keys to `.cli-flags.toml` env names or
route-map keys when those exist.

```sh
python3 scripts/check-generated-contract.py --freeze --require-readonly
```
