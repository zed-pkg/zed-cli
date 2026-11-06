# Zed as the default package manager for AttoLang — GA scorecard

**Status:** proposed acceptance criteria, not a claim that AttoLang packaging or all backends are production-ready. Public language name: **AttoLang**, formerly Oreslang. Existing `.ores` source files, `.ores.pkg.toml` root manifests and compatibility CLI paths remain supported until a separately reviewed migration. Canonical Zed package state remains `.zpkg.toml` and `.zpkg.lock`.

## End-to-end install contract

For a real published AttoLang library with at least one direct and one transitive dependency, run the following using a fresh home directory, isolated registry fixture and pinned compiler heads:

1. `zed init` / `zed add` / `zed install` resolve a single immutable dependency graph, write a content-addressed locked identity and retain exact source digest/provenance.
2. `zed attolang paths --json` projects only currently-installed, approved package roots. Legacy `zed oreslang paths` yields an equivalent normalized view. No network lookup, filesystem escape or unreviewed source path may be introduced by path projection.
3. A real compiler reads the installed module through the project-root alias and executes a `.atto` fixture that imports from the installed package. Preserve `.ores` compatibility.
4. `zed install --frozen` after deleting materialized project artifacts reconstructs byte-identical package bytes and compile output inputs. Offline restore succeeds only with every artifact already verified in the content-addressed cache.
5. A yanked or unavailable remote version does **not** silently rewrite the existing frozen lock. Missing digest, unexpected tag, corrupt archive, bad signature, symlink traversal or a version collision fails closed before changing the installed tree.
6. `zed publish` requires clean source, matching immutable tag/HEAD and verified artifact identity; publishing the same immutable version with different bytes is rejected. No arbitrary package build/install hook runs without reviewed explicit approval.

## Toolchain and compatibility matrix

At minimum test Java/GraalVM, LLVM, .NET NativeAOT, Wasm and Forge/Cranelift as separate lanes. Each lane distinguishes source syntax admission, compile success, native/guest execution, and signed artifact verification. A source-only conformance model is **not** compiler-backed guest proof.

AttoLang's canonical library sources should be byte-identical across backends. Compatibility tests cover the old `.ores` extension and `.ores.pkg.toml` semantics without adding an independent compiler resolver. Rust, TypeScript and Dart Zed interfaces must expose the same reviewed language/ecosystem identity.

## Quality, correctness, and performance scorecard

- **Correctness:** cold install, cache hit, frozen restore, offline restore, concurrent installers, crash after staged download/before atomic promote, partial graph failures and cyclic/unsatisfiable dependencies. Store content must never be partial or overwritten with a different digest.
- **Security:** forged registry response, artifact hash collision assumption disclosure, signature/attestation mismatch, malicious archive paths, absolute/symlink escape, auth failure, secret-safe logs, lockfile substitution, dependency confusion and malicious hook opt-in.
- **Determinism:** compare normalized graph, locked identities and assembled artifact digests across clean runs and macOS/Linux/Windows; record any platform-intended variance.
- **Benchmarks:** record cold and warm install latency, peak RSS, bytes downloaded, on-disk dedup footprint, CPU and complete-cache offline restore for 10/100/1,000-package synthetic graphs with a pinned public corpus. Compare against an identified baseline and version of Cargo/npm/pnpm where ecosystem semantics are actually comparable. **Do not claim superior performance without reproduced data.**
- **Developer UX:** unambiguous diagnostics for hash mismatch, unsupported backend, missing compiler, lock mismatch, untrusted build hook, registry 429/5xx, and import-root resolution. Machine-readable JSON output retains stable error classes.
- **Evidence:** exact PR head and source tree, binary/toolchain digests, nonempty executed steps, executed negative tests, host vs guest separation and independent review. Zero-step CI = `NOT_EXECUTED`.

## Current implementation dependencies

- Real read-only AttoLang package-path bridge: zed-cli **PR #543**.
- Canonical language/ecosystem plus Oreslang compatibility alias: zed-interfaces **PR #183**, tracking issue **#182**.
- Artifact/registry-to-source real end-to-end pipeline: zed-e2e **PR #72**, tracking issue **#71**.
- Two-writer immutable store safety model and pure `.atto` package oracle: zed-cli **PR #544** (this branch).

## Release gate

Do not label AttoLang package support as GA until the end-to-end contract is demonstrated for **all supported backends**, security negatives fail as expected, relevant contracts are merged and model-to-production refinement is evidenced. Source-only models and host smoke tests should remain separately named, even when green.
