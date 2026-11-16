# Universal `zed add` context and materialization policy

Zed is a **universal artifact/package manager**. It resolves package
identities, verifies downloaded bytes, maintains `.zpkg.toml` and
`.zpkg.lock`, and creates verified copy/symlink materializations in a
project's configured install tree. `zpkg.net` and its configured registry/CDN
mirrors serve package metadata/artifacts. Zed never needs to compile AttoLang,
Node.js, Rust, Python or any other language to decide which files to install.

## Three distinct decisions

1. **Zed authority:** the consumer directory owning `.zpkg.toml` and
   `.zpkg.lock`. The nearest ancestor Zed manifest is selected by default,
   or an exact one with `--project PATH`. Native manifests are not competing
   Zed lockfiles.
2. **Consumer target:** `[install].target` is the ordinary durable default;
   explicit `--target LANG` overrides it when safe; otherwise Zed uses one
   unambiguous native project hint.
   Native hints include `package.json`, `Cargo.toml`,
   `pyproject.toml`, `.atto.pkg.toml`, and compatibility
   `.ores.pkg.toml`. The legacy AttoLang marker should be migrated to
   `.atto.pkg.toml`; both at the same root are an error.
3. **Materialization:** `[install].dir` chooses the copy/symlink project
   directory, with optional established *wiring metadata* from the applicable
   adapter. **No compiler, package lifecycle hook, or native dependency
   installer runs just because a language manifest was detected.**
   Separate hook/build permissions still apply.

```text
mono/
├── .zpkg.toml                 # optional shared Zed consumer
├── .zpkg.lock
├── apps/
│   ├── site/
│   │   ├── .zpkg.toml        # optional independent Zed consumer
│   │   ├── package.json      # Node-owned semantics
│   │   └── src/
│   └── api/
│       ├── .zpkg.toml        # optional independent Zed consumer
│       ├── .atto.pkg.toml    # attoc-owned semantics
│       └── src/
```

```bash
# From an unambiguous Node member, use nearest Zed consumer and target
cd apps/site/src
zed add acme/http@^1

# From anywhere in the repository, name an exact consumer/target
zed add acme/actors@^1 --project apps/api --target attolang

# A root with two different native ecosystems must disambiguate
zed add acme/schema@^1 --project . --target nodejs
```

An existing Zed consumer uses `[install].target` with no flag, even if several
native manifests coexist. On `zed install`, `--target` can override this
for that invocation. On the **mutating** `zed add`, an explicit different
`--target` may switch the durable default **only if no dependencies are
already declared**: the successful add commits the new `[install].target`
with the added dependency, preserving subsequent frozen replay. With
existing dependencies, changing the target would reinterpret those packages;
Zed fails and asks for a separate member `.zpkg.toml` instead.
When no target is known and none is requested, the project remains universal;
Zed may install a universal package without assigning an arbitrary language.

**Current schema limitation:** `[dependencies]` is per Zed consumer, and
`[install].target` is a **consumer-wide** target, not a per-dependency
override. A polyglot monorepo whose members independently consume different
language-specific dependencies should use separate member `.zpkg.toml`
files (with workspace composition as appropriate). Do not silently mix
different target slices in one lock/install slot. A future
`[targets.<language>.dependencies]` contract would need shared-interface,
resolver and lockfile changes before being accepted as syntax.

## Context precedence and ambiguity

- The nearest `.zpkg.toml` on the current directory's ancestor chain wins
  for ownership. `--project` must name the exact Zed consumer; it never
  implicitly adopts another ancestor's dependencies.
- **Precedence:** explicit `--target` (subject to the durable-dependency
  safety rule) > `[install].target` > **nearest unique** native project
  marker. The `.zpkg.toml` default is authoritative even when a folder
  contains multiple different native-language manifests.
- Multiple native files representing the *same* ecosystem (`package.json`
  and `tsconfig.json`) are not ambiguous. Multiple distinct ecosystems
  at that level (`package.json` and `Cargo.toml`) are ambiguous.
- For invocation at the Zed root without a native hint, a bounded scan of
  nearby child projects may infer exactly one language. Separate nested
  Zed consumers, generated/vendor directories, hidden folders and symlinks
  are never searched as another consumer's target.
- A mixed source root can have `.zpkg.toml` **alongside**
  `package.json`, `.atto.pkg.toml`, etc. Those are not equivalent manifests;
  when the Zed manifest names `[install].target`, no ambiguous-language
  error is needed.
- An unresolved target blocks language-specific routing **before registry
  I/O or filesystem mutation**, with an actionable `--project` /
  `--target` diagnostic.
- No edits to `package.json`, `Cargo.toml`, `pyproject.toml`, or
  `.atto.pkg.toml` are implied by `zed add`. Any native manager adapter
  must honor the existing contract and its opt-in boundaries.

## AttoLang boundary

Zed materializes a verified source package. The AttoLang compiler **attoc**
then reads `.atto.pkg.toml`, resolves language imports and performs actual
compile/link/execute steps. Legacy `.ores.pkg.toml` is accepted only for
migration, not treated as a second dependency registry or package identity.
The current `zed attolang paths` bridge is a separate, read-only projection
under review; it must not become an Atto-specific fetch/compile mechanism.

## Test classification

`add_context` Rust tests prove root selection, mixed-manifest admission,
nested project behavior and explicit target controls **without registry
traffic**. They do not prove registry publication, checksum-verified
materialization, or successful native/AttoLang compilation. Those require
real isolated Zed+registry acceptance suites and independent guest tests.
