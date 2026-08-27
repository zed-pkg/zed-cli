# ores-lint

A vendored, dependency-free lint baseline for every JavaScript/TypeScript and
Rust repo in the org fleet. Everything it needs is in this directory — there is
nothing to install from a registry and nothing to keep in version sync.

## Running it

```sh
sh .ores-lint/lint.sh      # lints whatever this repo contains
sh .ores-lint/selftest.sh  # verifies the toolkit still works after a toolchain upgrade
```

For JS repos it is also wired into `npm run lint:ores`, and runs automatically
before `npm run build` (`prebuild`) and before `npm publish` (`prepublishOnly`).

## What it enforces

**JavaScript / TypeScript** — via the repo's own ESLint (flat config):

| rule | why |
|---|---|
| `semi` | house style: semicolons are required, missing ones warn |
| `ores/require-send` | a logging chain that reaches `.info()`/`.warn()`/… but never calls `.send()` builds an event that is never delivered |
| correctness set | `eqeqeq`, `no-unreachable`, `no-dupe-keys`, `use-isnan`, `valid-typeof`, `no-async-promise-executor`, and similar low-false-positive checks |

**Rust** — via clippy:

| lint | why |
|---|---|
| `clippy::implicit_return` | house style: prefer an explicit `return` at tail position |
| `clippy::correctness`, `clippy::suspicious` | real defects |
| `unwrap_used`, `expect_used`, `panic_in_result_fn`, `todo`, `dbg_macro` | things that should not reach a publish |

## The two things worth knowing

**1. Implicit-return findings are capped.** `clippy::implicit_return` fires once
per implicit return, which on a real crate is hundreds of identical lines. The
lint stays fully enabled so nothing is missed, but `rust.sh` collapses it into a
single warning showing at most 5 concrete locations plus `... and N more`. The
same cap applies to every ESLint rule via `eslint/formatter.mjs`. Change it with
`ORES_LINT_MAX_EXAMPLES`.

**2. `clippy::needless_return` had to be disabled.** It ships enabled in
clippy's default `style` group and warns on exactly the explicit returns this
house style asks for. Enabling `implicit_return` without allowing
`needless_return` makes the two lints contradict each other on every function in
the crate. `selftest.sh` asserts this stays true.

## Scope: sub-projects and repo boundaries

The linter does **not** assume the repo root is the only project.

- **Rust** — `rust.sh` finds every crate in the repo, including ones under
  `apps/` or `clients/`. Crates that are workspace members of an already-linted
  root are skipped (via `cargo metadata --no-deps`) so nothing is linted twice,
  and findings from every crate are aggregated into **one** capped report.
- **JS/TS** — a flat config at the repo root makes `eslint .` reach nested
  packages, so the config goes in even when the JS lives in a subdirectory.

**Nested git repositories are a hard boundary.** A repo checked out inside
another repo gets its own ores-lint install; the parent must not lint it, or the
same findings get reported twice under the wrong repo name and the same
`package.json` gets wired with two conflicting relative paths. `rollout.mjs`
records those boundaries in `.ores-lint/nested-repos.json`, and both halves of
the linter read it.

To exclude a repo entirely — vendored upstream forks, for instance — drop an
empty `.ores-lint-ignore` file at its root.

## Legacy config migration

ESLint 9+ reads flat config **only**. Three older mechanisms are silently
ignored, which means any repo still relying on them has not been linted at all:

| legacy mechanism | status |
|---|---|
| `.eslintrc*` | ignored entirely; rules are dead |
| `eslintConfig` key in `package.json` | ignored entirely |
| `.eslintignore` | ignored, with a warning |

`audit.mjs` reports every repo in each category. `.eslintignore` is ported
automatically into flat-config `ignores` by `base.mjs` (gitignore semantics
preserved), so its intent keeps applying. The other two need a human decision
and are migrated per repo — porting the rules that still make sense, and saying
in a comment which ones were dropped and why.

## Warn-only, by design

`lint.sh` exits 0 no matter what it finds. It is wired into build and publish
hooks across hundreds of repos, so it is built to be incapable of breaking one
unless a human opts in.

To make findings blocking for a single repo, create `.ores-lint/local.sh`:

```sh
ORES_LINT_STRICT=1
```

`local.sh` is yours — the rollout script never overwrites it. Everything else in
this directory is managed and will be replaced on the next rollout.

## Knobs

| variable | default | meaning |
|---|---|---|
| `ORES_LINT_MAX_EXAMPLES` | `5` | example locations shown per rule |
| `ORES_LINT_STRICT` | `0` | `1` makes any finding exit non-zero |
| `ORES_LINT_SKIP_JS` / `ORES_LINT_SKIP_RUST` | `0` | skip one half |
| `ORES_LINT_RUST_ALL_TARGETS` | `0` | `1` also lints tests/benches/examples |
| `ORES_LINT_RUST_EXTRA` | — | extra flags appended to the clippy invocation |

## Graceful degradation

Nothing here is allowed to fail loudly for an environmental reason. ESLint not
installed, too old, clippy not installed, no TypeScript parser available, crate
deps not fetchable — each is reported as an actionable skip, not an error.
Repo-specific ESLint config that already existed is never overwritten.

CI follows the same model: the workflow runs `npm i -g eslint typescript-eslint`
and never runs `npm install` for the repo itself, so linting a PR does not
require the repo's dependency tree to resolve.

## Per-repo customisation

`eslint.config.mjs` at the repo root takes options:

```js
export default await oresConfig({
  requireSend: { loggerNames: ['myLogger'], terminalMethods: ['send', 'flush'] },
  rules: { 'no-console': 'warn' },
  ignores: ['**/generated/**'],
});
```

Once you edit that file the rollout script leaves it alone.

---

## Fleet operations (from the `codes` directory)

```sh
node .ores-lint-toolkit/audit.mjs                 # report the fleet's lint posture
node .ores-lint-toolkit/audit.mjs --json out.json # ...as machine-readable data
node .ores-lint-toolkit/rollout.mjs --dry-run     # preview
node .ores-lint-toolkit/rollout.mjs               # install / re-install everywhere
node .ores-lint-toolkit/rollout.mjs --only ores-otel
node .ores-lint-toolkit/rollout.mjs --shard 0/8   # one slice of a fleet-wide run
node .ores-lint-toolkit/verify.mjs                # assert every repo is correctly installed
```

A full rollout over ~900 repos takes a few minutes. `--shard k/n` splits it into
bounded chunks, which matters when the runner has a per-command time limit.

Re-run the rollout after editing anything in `.ores-lint-toolkit/` — it is
idempotent and propagates the change to every repo.
