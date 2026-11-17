# AttoLang local-registry fixture

This fixture has a Zed publisher manifest at the root and an independent
compiler-owned `.atto.pkg.toml` inside the `attolang/` target slice. Zed
publishes the target as `zed-pkg/attolang-fixture-lib-attolang`; consumers
select it with `[install].target = "attolang"`.

The durable-install integration script installs this package into a consumer
that also has both `.zpkg.toml` and `.atto.pkg.toml`. It validates file
materialization and frozen lock replay only; the Atto compiler is intentionally
not invoked by Zed.
