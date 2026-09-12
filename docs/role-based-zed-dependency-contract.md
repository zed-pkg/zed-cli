# Role-based Zed dependency contract

`tools/role-contract/zed_role_contract.py` inventories a local cohort of Zed package manifests, validates the canonical role graph, and can transactionally repair missing or stale role dependencies.

The tool reads producer identity and version directly from each producer's `.zpkg.toml`. It never guesses an organization, package name, source, or version.

## Role metadata

Canonical suffixes are recognized as a compatibility heuristic. Repositories with ambiguous names must declare both fields in `[package]`:

```toml
[package]
role = "server"
family = "example"
```

Supported normalized roles are `interfaces`, `clients`, `lib`, `server`, `cli`, `mcp`, and `e2e`. `*-libs` normalizes to `lib`; API and web servers normalize to `server`.

## Audit and check

```sh
python3 tools/role-contract/zed_role_contract.py audit /path/to/cohort \
  --output role-audit.json
python3 tools/role-contract/zed_role_contract.py check /path/to/cohort
```

`audit` always emits the machine-readable inventory and findings. `check` exits 2 when any blocker exists. Neither mode mutates manifests or lockfiles.

## Transactional fix

Preview exact edits and lock-refresh commands:

```sh
python3 tools/role-contract/zed_role_contract.py fix /path/to/cohort \
  --dry-run \
  --output role-fix-plan.json
```

Apply the plan and refresh every affected lock with the installed Zed CLI:

```sh
python3 tools/role-contract/zed_role_contract.py fix /path/to/cohort \
  --lock-command "zed install" \
  --output role-fix-result.json
```

`fix` only edits `DEPENDENCY_MISSING` and `DEPENDENCY_CONSTRAINT_STALE` findings backed by exact producer manifests in the scanned cohort. It refuses to mutate when it sees an ambiguous role, missing producer, role conflict, malformed manifest, invalid dependency value, or dependency cycle.

The editor preserves comments, unknown tables, key spelling, whitespace around existing assignments, and inline comments. Missing dependencies are inserted into the existing `[dependencies]` table or a new table is appended. Every revised document is parsed again before it is written.

Manifest and lock changes form one cohort transaction:

1. snapshot every affected `.zpkg.toml` and `.zpkg.lock`;
2. atomically replace the revised manifests;
3. run the configured lock command in each changed package root;
4. require a resulting `.zpkg.lock` in every package;
5. re-audit the complete cohort;
6. restore every manifest and lock snapshot if any command or validation fails.

A successful second `fix` run is a no-op and does not run the lock command again. This makes the command suitable for pre-build checks, controlled fleet migrations, and CI idempotency tests.

The default lock command is `zed install`. Automated migrations should pass an exact trusted executable or wrapper through `--lock-command` so the lock-refresh implementation is explicit in logs and reproducible across environments.
