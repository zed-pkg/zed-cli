# GitHub interface fleet audit

`tools/role-contract/github_interface_fleet.py` discovers `*-interfaces` producers and their matching `*-web-server.rs` and `*-api-server.rs` consumers across one or more GitHub organizations.

The scanner is read-only and fail-closed. A server is not considered integrated merely because `.zpkg.toml` names the interface package. A conformant consumer must have:

- `package.role = "server"` and the same `package.family` as the producer;
- a Zed dependency on the exact `ORG/FAMILY-interfaces` coordinate;
- a resolver-created `.zpkg.lock`;
- Rust source that imports the generated interface crate.

The producer must declare `package.role = "interfaces"`, the inferred family, and no dependency on another family producer.

## Deterministic snapshots

CI and offline audits can consume a JSON snapshot containing repository names and selected file contents:

```sh
python3 tools/role-contract/github_interface_fleet.py \
  --snapshot fleet-snapshot.json \
  --strict \
  --output fleet-audit.json
```

## Live organization audit

A live deep audit reads repository metadata, Zed manifests and locks, Cargo manifests, and Rust source files through the GitHub REST API:

```sh
GITHUB_TOKEN=... python3 tools/role-contract/github_interface_fleet.py \
  --org ecma-d \
  --org canonical-cloud \
  --org shared-auth \
  --org messaging-intel \
  --deep \
  --strict \
  --output fleet-audit.json
```

Use a read-only GitHub App installation token scoped to the audited organizations. Do not put a token in a manifest, command history, report, or repository URL.

## Repair workflow

The fleet scanner identifies remote cohorts. Clone the relevant producer and consumers into one local cohort, then apply the transactional role fixer:

```sh
python3 tools/role-contract/zed_role_contract.py audit ./cohort --output role-audit.json
python3 tools/role-contract/zed_role_contract.py fix ./cohort --dry-run
python3 tools/role-contract/zed_role_contract.py fix ./cohort \
  --lock-command 'zed install --locked'
python3 tools/role-contract/zed_role_contract.py check ./cohort
```

The fixer snapshots every affected `.zpkg.toml` and `.zpkg.lock`. It restores them byte-for-byte if dependency resolution, compilation, post-fix audit, or idempotence fails.

Source-level HTTP/RPC conversion remains repository-specific. The scanner deliberately reports a manifest-only dependency as nonconformant so automation cannot disguise duplicate hand-written DTOs as generated-interface adoption.
