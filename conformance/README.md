# Conformance

`zed-cli` dogfoods the same `contracts/` + `conformance/` lifecycle boundary it enforces for package consumers.

The local `contracts/` files are immutable bindings to the exact `zed-interfaces` and `zed-lib-core` revisions consumed by this crate. `conformance/check.mjs` fails if those bindings diverge from `Cargo.toml`, then runs the shared fail-closed digest/symlink/evidence boundary.

Coverage starts `scaffold-only`; bootstrap metadata is not behavioral coverage. Existing Zed integration/unit suites remain the implementation proof until shared behavioral cases are promoted here with explicit participant evidence.
