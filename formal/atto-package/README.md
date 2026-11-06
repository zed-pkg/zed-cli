# AttoLang / Zed package safety extension

This separate model complements, and does not replace, `formal/review-procedure/model.py` or the in-flight native AttoLang bridge (zed-cli PR #543).

- `two_writer_model.cjs`: deterministic bounded exploration (12 transitions, two writers, matching/tampered downloads, crash during staging, immutable-store race, frozen install).
- `package_safety.atto`: **AttoLang source-level predicates** for digest/provenance/frozen-install/publish admission. It is not compiled or executed by the Node workflow.
- `atto-package-safety.yml`: executes the bounded model on affected PRs. A green run is **not** proof of compiler/runtime parity or an arbitrary number of installers.

Safety: no tampered artifact enters the canonical store; a project link points only at a committed matching store entry; a racing second writer cannot overwrite it; a crashed writer's staging is discarded. The model requires positive and negative witnesses and attempts three deliberate invariant mutations.

Next refinement gates before claiming first-class AttoLang support:
1. Land or reconcile zed-cli #543's real `.atto`/manifest/root-alias handling; test `.ores.pkg.toml` root compatibility and `.zpkg.toml` package identity.
2. Reproduce model schedules against Rust store transaction APIs and actual temporary filesystems, including symlink attacks and power-loss boundaries.
3. Compile and run this exact `.atto` policy on each supported AttoLang backend, with exact head, executed CI steps and independent review.
4. Exercise install/offline/frozen/publish from a real AttoLang consumer, with immutable hashes and signed provenance receipts.
