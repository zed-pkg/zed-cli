# Role-based Zed dependency contract

`tools/role-contract/zed_role_contract.py` inventories a local cohort of Zed package
manifests and validates the canonical role graph without network access or file
mutation.

The tool reads producer identity and version directly from each producer's
`.zpkg.toml`. A missing producer is a blocker; the tool never guesses an
organization, package name, source, or version.

## Role metadata

Canonical suffixes are recognized as a compatibility heuristic. Repositories with
ambiguous names must declare both fields in `[package]`:

```toml
[package]
role = "server"
family = "example"
```

Supported normalized roles are `interfaces`, `clients`, `lib`, `server`, `cli`,
`mcp`, and `e2e`. `*-libs` normalizes to `lib`; API and web servers normalize to
`server`.

## Modes

```sh
python3 tools/role-contract/zed_role_contract.py audit /path/to/cohort \
  --output role-audit.json
python3 tools/role-contract/zed_role_contract.py check /path/to/cohort
```

`audit` always emits the machine-readable inventory and findings. `check` emits the
same report and exits 2 when any blocker exists. Neither mode mutates manifests or
lockfiles.

The follow-up mutation slice must preserve comments and unknown fields, update
manifest and lock atomically, require exact scanned producer coordinates, and make a
second `fix` run a no-op. That write path is deliberately not approximated by this
non-mutating foundation.
