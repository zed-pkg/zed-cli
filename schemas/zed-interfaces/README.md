# Pinned package descriptor schemas

`manifest.json` and `lockfile.json` are byte-for-byte copies of the generated
schemas at zed-interfaces commit
`4b87e425b04777b0ee413971dc1df805d24f295f`, the exact revision pinned in
`Cargo.toml` and `Cargo.lock`.

`zed validate` uses these files only to close the canonical object shapes and
reject unknown fields. The Rust types and runtime validators from that same
zed-interfaces revision remain authoritative for TOML decoding and semantic
validation. Zed CLI's additive `[[git-submodule]]` extension is removed from
the canonical schema view and validated separately by the Git-submodule
subsystem.

When the zed-interfaces pin changes, replace both files from that exact commit
and run the validator schema-pin test before committing the dependency update.
