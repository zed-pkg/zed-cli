# CLI package dependency graph

`zed-cli` is both a native Rust crate and a Zed package. Those manifests serve
different package managers, but they must describe the same architectural
layers.

## Required Zed edges

The root `.zpkg.toml` imports:

- `zed-pkg/zed-interfaces`, the shared manifest, protocol, resolution, and
  package-model contract;
- `zed-pkg/zed-clients-rust`, the Rust target exposed by the polyglot clients
  repository.

There is no `zed-lib` repository today. If that reusable layer is introduced,
its package becomes a required dependency here and in the organization fleet
audit.

The CLI never imports `zed-infra`. Infrastructure remains an operational
repository and may be present only in integration/portfolio composition.

## Native Rust edge

`Cargo.toml` retains the direct `zed-interfaces` dependency used throughout the
implementation. The clients repository is represented as a Zed target edge so
Zed can resolve, install, and verify the reusable client package as part of the
cross-repository graph. Native client-code consolidation can proceed without
hiding that architectural dependency or coupling standalone Cargo builds to an
unavailable sibling path.

## CI

`.github/workflows/zed-package-edges.yml` checks out the CLI, interfaces, and
clients repositories as siblings, then runs:

```sh
python3 scripts/check-zed-package-edges.py \
  --interfaces-manifest ../zed-interfaces/.zpkg.toml \
  --clients-manifest ../zed-clients/.zpkg.toml
```

The check verifies exact Zed dependency keys, package URLs, lockfile format,
the native interfaces dependency, and the existence of the clients Rust target.
