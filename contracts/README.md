# Contracts

This directory is the repository-local contract boundary for `zed-cli`.

Shared package/lock/lifecycle shapes remain owned by `zed-pkg/zed-interfaces`; checked-in schema snapshots under `schemas/zed-interfaces/` are consumer evidence pinned to that authority, not a second source of truth. Any `zed-cli`-specific TypeSpec and JSON Schema Draft 2020-12 contracts added here must remain independent, human-authored peers and must be exercised from `conformance/`.

`contracts/` is intentionally part of the Zed lifecycle gate. A repository that adopts either the `contracts/` or `conformance/` boundary must keep both roots as real directories rather than symlinks.
