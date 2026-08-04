# zed-cli

`zed` is the CLI for [zed-pkg](https://zpkg.tech), the universal package
manager backed by the VCS hosts you already use. Artifacts live on the
zpkg.tech registry (S3/Cloudflare R2 behind Rust servers); your declared
backing repo on GitHub, GitLab, Bitbucket, Codeberg, SourceHut, Forgejo,
Gitea, or a self-hosted server doubles as mirror and provenance anchor.

Why it exists:

- **Install packages, not repositories.** No cloning entire repos (tests
  included) onto laptops and servers. Published artifacts are pruned: tests,
  CI config, `.github/`, and READMEs are stripped by default. Licenses are
  always kept.
- **pnpm philosophy.** One content-addressed copy per machine under
  `~/.zed-pkg/store`, symlinked into each project's `zed_modules/`. No more
  hefty per-project dependency folders.
- **Provenance by tags.** Publishing requires a VCS tag matching the version
  (`v{version}` by default) pointing at the exact published commit; the tag
  and commit are pinned in `.zpkg.lock`.
- **Container-first.** The documented [copy install ownership contract](docs/install-modes.md)
  materializes independent package, adapter, build-output, and hoisted-bin files
  for Docker build contexts, OCI layers, and read-only runtimes.

## Install

```sh
curl -fsSL https://zpkg.tech/install.sh | bash
```

The installer detects your OS/arch, drops the `zed` binary in `~/.zed/bin`,
and adds it to your `PATH` idempotently. Or via Homebrew:

```sh
brew tap zed-pkg/tap
brew install zed-pkg
```

Or from source:

```sh
cargo install --path .
```

Keep zed current with `zed self-update` (checks the latest GitHub release for
your platform and replaces the binary in place; `--check` reports only).

Note: the Zed editor also installs a `zed` binary. The Homebrew formula
declares the conflict; if you use both, install with
`cargo install --path . --root ~/.zed-pkg-cli` and alias as you like.

## Quickstart

```sh
# author a package
cd my-lib
zed init --org acme
git tag v0.1.0
zed r2g               # consume your own artifact before shipping (add --docker for a container)
zed publish

# derive the exact immutable OCI identity without credentials or uploads
zed oci plan oci://ghcr.io/acme/my-lib:0.1.0

# consume packages from a manifest
zed add acme/http-kit@^1
zed install
zed uninstall                          # remove files; keep the exact lock
zed install --frozen                   # restore the same artifacts

# or install transiently in a folder with no .zpkg.toml
zed install acme/http-kit@^1            # confirms in an interactive terminal
zed install acme/http-kit@^1 --skip-manifest  # intentional automation
zed find http

# opt into a confirmation at every mutating lifecycle step
zed install --interactive
zed r2g --docker --interactive
zed publish --interactive
```

Every authored package is `<org>/<name>`, declared in a `.zpkg.toml` manifest
at the repo root (TOML only). Consumers may also install positional package specs
through a transient in-memory manifest. See `zed init` output for the annotated
authoring template.

## Versioning

Packages declare a `version_scheme` (default `semver`):

- **semver** — `1.2.3`; ranges (`^1.2`, `>=0.2 <0.5`) resolve to the max
  satisfying stable version.
- **calver** — calendar versions (`2026.07.24`); normalized to a semver total
  order so the same range algebra applies (`>=2026.0.0 <2027.0.0`).
- **opaque** — arbitrary tags (`legacy-api`); a requirement must match a
  published version exactly.

Foreign tag spellings are tolerated on resolution: a leading `v`, Go's
`+incompatible`, and common PEP 440 pre-releases all normalize to a comparable
version. See [zed-interfaces `version`](https://github.com/zed-pkg/zed-interfaces/blob/main/src/version.rs).

## Artifact formats

Artifacts are `tar.gz` by default; `zip` is fully supported (both pack
deterministically and install through the store's magic-byte extraction). The
registry hosts both on S3/Cloudflare R2.

## Commands

| Command | What it does |
| --- | --- |
| `zed init` | Write a `.zpkg.toml` template |
| `zed add <org>/<name>[@req]` | Add a dependency and install |
| `zed remove <org>/<name>` | Remove a dependency |
| `zed install [<org>/<name>[@req] ...]` (`zed i`) | Resolve, download once into the store, and install manifest or transient dependencies |
| `zed install --frozen` | Install exactly what `.zpkg.lock` pins (CI/containers, including manifestless locked reinstalls) |
| `zed uninstall [<org>/<name> ...]` (`zed un`) | Transactionally remove all or selected materialized packages while retaining the manifest and lockfile for a frozen reinstall |
| `zed find <query>` | Search the registry |
| `zed pack` | Build the pruned, deterministic `tar.gz` artifact |
| `zed release plan [--json]` | Print the credential-free Zed, native-registry, and forge-package release set derived from `.zpkg.toml` |
| `zed release preflight` | Validate native manifests, then run fixed credential-free package preflight adapters |
| `zed oci plan <oci://registry/repository:version> [--target <name>] [--json]` | Pack in a temporary directory and print exact OCI config, layer, manifest, and resolved digest identities without credentials, network, or uploads |
| `zed oci push <layout> <oci://registry/repository:version>` | Verify an immutable local OCI layout, copy it through ORAS with one explicit authentication mode, and require the remote tag to resolve to the verified manifest digest |
| `zed publish` | Verify clean tree + matching VCS tag at HEAD, pack, upload |
| `zed r2g` (`zed test-local`) | Roundtrip-test your artifact: install it into a mock consumer under `~/.zed-pkg/r2g` and run `publish.smoke_test`, optionally inside an OCI container (`--docker`) |
| `zed run <bin> [args]` | Run an executable a dependency exposes via `[bin]`, with `zed_modules/.bin` on `PATH` (npx-style, no global pollution) |
| `zed build [--force]` | Run (or warm the cache for) dependencies' `[build]` steps |
| `zed yank <org>/<name>@<version> [--undo]` | Hide a version from fresh resolution (existing lockfiles keep working) |
| `zed login` / `zed signin` | Sign in (`zed auth login` / `zed auth signin` are identical) |
| `zed signup` / `zed register` | Create an account (`zed auth signup` / `zed auth register` are identical) |
| `zed logout` / `zed signout` | Revoke and remove the session (`zed auth logout` / `zed auth signout` are identical) |
| `zed auth status` | Show the current account, authorities, and JWT expiries |
| `zed auth refresh` | Rotate shared-auth and Supabase refresh tokens now |
| `zed auth token` | Print the preferred current access JWT for scripting |
| `zed auth import-token` | Save a legacy opaque registry token to `credentials.toml` |
| `zed org claim <slug>` | Claim a namespace |
| `zed org audit <slug> [--limit N]` | Read the org's audit log — who changed published state, newest first (server registries only; needs an `owner` token) |
| `zed store status\|path\|prune` | Inspect the store or prune unreferenced entries |
| `zed gc [--older-than 90d] [--dry-run]` | LRU collection: drop store/build entries no live project references and unused past the cutoff, plus stale downloads |
| `zed cache clean` | Drop cached downloads |
| `zed self-update [--check] [--force]` | Replace the binary with the latest GitHub release for your platform |
| `zed completions bash\|zsh` | Generate shell completion from the same Clap model used by the executable |

### Shell completion

```sh
# Bash for the current shell
source <(zed completions bash)

# Zsh (persistent user completion)
mkdir -p ~/.zfunc
zed completions zsh > ~/.zfunc/_zed
fpath=(~/.zfunc $fpath)
autoload -Uz compinit && compinit
```

The generated scripts include aliases, subcommands, and install flags directly
from the typed parser. GitHub Actions syntax-checks and registers them in real
Bash and Zsh processes.

### Native and forge package releases

A polyglot target can name its canonical ecosystem registry and optional
copies in package registries operated by GitHub, GitLab, or Bitbucket:

```toml
[targets.nodejs]
dir = "clients/typescript"

[targets.nodejs.native]
registry = "npm"
package = "@acme/client"
forge = ["github-packages", "gitlab-packages", "bitbucket-packages"]
```

A single-language repository uses `[publish.native]` with the same `registry`,
`package`, and `forge` fields; its native package-manager manifest is read from
the repository root.

Tag-resolved packages can add a native `tag_format`. Go modules below a
subdirectory must use the module-directory prefix, such as
`tag_format = "clients/go/v{version}"`, while the coordinated Zed release can
continue using the repository tag `v{version}`.

`zed release plan --json` emits one coordinated release set containing the
Zed artifacts, canonical native packages, and forge mirrors. It does not read
credentials or upload. `zed release preflight` verifies the native package
identity/version and runs the ecosystem's packaging command without publishing.

The manifest rejects unsupported combinations before CI reaches credentials:
GitHub Packages accepts npm, Maven, RubyGems, and NuGet routes; GitLab also
accepts PyPI, Packagist/Composer, and Go module routes; Bitbucket Packages
accepts npm and Maven routes. Cargo and pub.dev remain canonical-native plus
Zed destinations because those forges do not expose matching registry
protocols.

### Installing without `.zpkg.toml`

`zed install` accepts transient package specs in an existing repository or
folder:

```sh
zed install oresoftware/flags-2-env@^0.1
```

Zed first searches upward for a Zed manifest. Without one, it looks for the
nearest native project marker (`package.json`, `Cargo.toml`, `go.mod`,
`pyproject.toml`, and other supported ecosystems). When invoked at a repository
shell containing exactly one clear nested app such as `apps/web/package.json`,
that app becomes the install root. Ambiguous monorepos stay at the requested
root and use the safe universal `zed_modules/` layout rather than guessing.

A real interactive terminal prints the selected root, inferred target, adapter,
and dependencies, then accepts only `y` or `yes`. EOF and every other answer
cancel before files are written. Automation must opt in with
`--allow-no-manifest`, its visible alias `--skip-manifest`, or
`ZED_PKG_ALLOW_NO_MANIFEST=1`.

No synthetic `.zpkg.toml` is written. The normal installer still writes
`.zpkg.lock`, `zed_modules/`, hoisted bins, and supported ecosystem adapter
outputs. `zed install --frozen --skip-manifest` can reconstruct a no-manifest
install from an existing lockfile without package operands. In a project that
already has a manifest, use `zed add` to persist a dependency; positional
package operands on `zed install` are rejected rather than silently creating a
non-persistent manifest override.

### Where dependencies land (`[install].dir`)

zed complements npm/maven/etc. rather than replacing them, so its (few,
hand-picked) dependencies live in their own tree alongside the native one.
That tree defaults to `zed_modules/` and can be relocated:

```toml
[install]
dir = ".vendor/.zed"     # default: zed_modules
adapter = "node"         # optional; omitted = auto-detect (or --adapter)
```

Every command that touches the tree honors this: `zed install` writes it,
`zed uninstall` removes all or selected materialized packages, `zed run`
finds hoisted bins in `<dir>/.bin/`, `zed remove` updates the manifest,
and `zed pack`/`zed publish` always exclude it — a relocated dependency tree
is never published (see `a_relocated_install_dir_is_never_published`).

### Interactive transactions and recovery

`--interactive` is a global opt-in flag (`ZED_PKG_INTERACTIVE=1`) declared in
the same `.cli-flags.toml` contract audited by flags-2-env. Mutating lifecycle
commands put confirmations immediately before the steps they own: package
materialization, lock/ref updates, each publish upload, and each r2g phase.
A declined answer, EOF, or redirected stdin fails closed.

Install and uninstall protect project-tree changes with durable UUID-v4
transactions under `.zpkg-staging/<uuid>/`. Replaced paths are renamed into
the staging area before mutation. Normal errors restore them immediately; a
hard exit leaves the transaction metadata in place, and the next Zed
lifecycle invocation recovers it before starting new work. Successful
transactions remove their staging directory, and packing always excludes
`.zpkg-staging/**`.

### Monorepo workspaces

A root manifest with a `[workspace]` table links member packages from source
instead of the registry, so edits are live in consumers with no publish step:

```toml
# .zpkg.toml at the monorepo root
[workspace]
members = ["packages/*", "apps/*"]
```

When a dependency resolves to a workspace member, `zed install` symlinks the
member's source directory straight into `zed_modules/` and keeps resolving its
transitive deps. Members are not pinned in `.zpkg.lock` (there is no artifact).

### Build hooks (compiled dependencies)

A package with native code or a codegen step declares a `[build]`:

```toml
[build]
command = "cargo build --release"
outputs = ["target/release/libfoo.so"]   # empty = keep the whole tree

[build-dependencies]                       # tools needed only during the build
"acme/cmake" = "^3.20"
```

Builds run in an isolated staging copy — never inside the immutable source
store — and results cache per `(sha256, platform, command)` under
`~/.zed-pkg/builds/`, so a consumer override never collides with the
package's own build.
Because a build runs arbitrary author code, it is opt-in: pass `--allow-build`
(or set `ZED_PKG_ALLOW_BUILD=1`). A consumer can patch or replace a
dependency's build without waiting on upstream:

```toml
[overrides.build."acme/crypto"]
command = "make install CC=clang"
```

## Flags-2-env

Following the [flags-2-env](https://github.com/oresoftware/flags-2-env)
convention, every flag can be set via a `ZED_PKG_*` environment variable. The
full mapping is declared, TOML-only, in
[`.cli-flags.toml`](.cli-flags.toml) — a `cargo test` asserts that file and the
actual CLI never drift, so it is always authoritative:

| Flag | Env var | Default |
| --- | --- | --- |
| `--registry` | `ZED_PKG_REGISTRY` | `https://registry.zpkg.tech` |
| `--home` | `ZED_PKG_HOME` | `~/.zed-pkg` |
| `--token` | `ZED_PKG_TOKEN` | saved credentials |
| `--auth-url` | `ZED_PKG_AUTH_URL` | `<registry>/shared-auth` |
| `--supabase-url` | `ZED_PKG_SUPABASE_URL` | optional Supabase project URL |
| `--supabase-key` | `ZED_PKG_SUPABASE_KEY` | optional public publishable/anon key |
| `--interactive` | `ZED_PKG_INTERACTIVE` | off; confirm each mutating lifecycle step in a real terminal |
| `--install-mode` | `ZED_PKG_INSTALL_MODE` | `symlink` |
| `--adapter` | `ZED_PKG_ADAPTER` | `auto` — context-aware linking: `package.json` projects also get `node_modules/@org/name` links; `pom.xml`/`build.gradle` projects get a generated `.zed/classpath` of installed jars for `java -cp "$(cat .zed/classpath)"`; python site-packages planned |
| `--frozen` | `ZED_PKG_FROZEN` | off |
| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |
| `--allow-no-manifest` / `--skip-manifest` (install) | `ZED_PKG_ALLOW_NO_MANIFEST` | off; otherwise a real-terminal confirmation is required |
| `--target` (OCI plan/polyglot install) | `ZED_PKG_TARGET` | required for a polyglot OCI plan; inferred for install when possible |
| `--json` (OCI plan) | `ZED_PKG_OCI_JSON` | off |
| `--force` (build) | `ZED_PKG_FORCE` | off |
| `--older-than` (gc) | `ZED_PKG_GC_OLDER_THAN` | `90d` |
| `--dry-run` (gc) | `ZED_PKG_GC_DRY_RUN` | off |
| `--dry-run` (publish) | `ZED_PKG_DRY_RUN` | off |
| `--allow-dirty` | `ZED_PKG_ALLOW_DIRTY` | off |
| `--skip-vcs-checks` | `ZED_PKG_SKIP_VCS_CHECKS` | off |
| `--undo` (yank) | `ZED_PKG_YANK_UNDO` | off |
| `--out` (pack) | `ZED_PKG_PACK_OUT` | `.zed/pack` |
| `--org` (init) | `ZED_PKG_ORG` | - |
| `--name` (init) | `ZED_PKG_NAME` | directory name |
| `--check` (self-update) | `ZED_PKG_UPDATE_CHECK` | off |
| `--force` (self-update) | `ZED_PKG_UPDATE_FORCE` | off |
| `--docker` (r2g) | `ZED_PKG_R2G_DOCKER` | off |
| `--image` (r2g) | `ZED_PKG_R2G_IMAGE` | `debian:stable-slim` |
| `--runtime` (r2g) | `ZED_PKG_R2G_RUNTIME` | auto (docker, then podman) |
| `--r2g-root` (r2g) | `ZED_PKG_R2G_ROOT` | `<home>/r2g` |
| `--clean` (r2g) | `ZED_PKG_R2G_CLEAN` | off |

`--registry file:///path` selects a directory-backed registry: hermetic CI,
air-gapped mirrors, and `zed r2g` all use it.

## Authentication

`zed login` and `zed auth login` are the same operation. With
`ZED_PKG_SUPABASE_URL` and the public `ZED_PKG_SUPABASE_KEY` configured, zed
uses Supabase Auth for the credential exchange and then exchanges that provider
JWT at shared-auth. It retains both independently refreshable sessions:
shared-auth is preferred, while the Supabase JWT remains available as the
dual-auth fallback. Without Supabase configuration, login and registration use
shared-auth directly.

Passwords are read from a hidden terminal prompt and never stored. For
non-interactive use, pass `--password-stdin` or inject
`ZED_PKG_AUTH_PASSWORD`. Access and rotating refresh tokens are stored in
`~/.zed-pkg/auth/sessions.toml`; the directory is mode `0700` and the file is
mode `0600` on Unix. `zed logout` attempts revocation at both authorities and
always removes the local session.

## Containers & OCI

### Immutable OCI publication plans

`zed oci plan` derives the exact OCI artifact bytes and identities before any
registry is contacted:

```sh
zed oci plan oci://ghcr.io/acme/tool:1.2.3
zed oci plan oci://ghcr.io/acme/tool-rust:1.2.3 --target rust --json
```

The input is a tagged destination. Contract v1 requires that tag to equal the
package version and rejects a caller-supplied digest. Zed validates the source
manifest and frozen dependency provenance, packs in a temporary directory, and
hashes the package archive, Zed manifest, optional lockfile, config JSON, and
OCI image manifest. The output includes the resolved immutable
`oci://...@sha256:...` reference. Planning reads no registry credentials,
performs no network request, uploads nothing, and leaves no `.zed/pack` output.
A later transport command can consume the same descriptors without changing
what is signed, attested, or pushed.

Symlinks into `$HOME/.zed-pkg` do not survive a `COPY --from=build` between
image stages, so use copy mode inside builds and cache-mount the store:

```dockerfile
FROM rust:1-slim AS build
RUN cargo install zed-cli --root /usr/local            # or COPY a prebuilt zed
WORKDIR /app
COPY .zpkg.toml .zpkg.lock ./
RUN --mount=type=cache,target=/root/.zed-pkg \
    zed install --frozen --install-mode copy
COPY . .
RUN make build

FROM gcr.io/distroless/cc
COPY --from=build /app/out /app
```

- `--frozen` keeps builds reproducible: exactly the sha256s in `.zpkg.lock`.
- `--install-mode copy` materializes files so the layer is self-contained;
  the cache mount still deduplicates downloads across builds.
- Artifacts are pre-pruned at publish time, so images stay small without
  extra cleanup steps.

The test suite asserts copy-mode installs contain zero symlinks; run the
whole suite inside a clean container with `scripts/container-smoke.sh`.

## r2g (test-local)

Named after [r2g](https://github.com/oresoftware/r2g): the failure mode it
kills is "works in my repo, breaks when installed." Instead of testing your
working tree, `zed r2g` exercises the *published artifact* the way a real
consumer would:

1. **Pack** — builds the exact pruned, deterministic tarball `zed publish`
   would upload.
2. **Publish** — to a throwaway `file://` registry.
3. **Install** — into a mock consumer project (`zed-local/consumer`) with its
   own throwaway store, so the tarball actually roundtrips through
   extraction — no reaching back into your source tree.
4. **Smoke test** — runs your `publish.smoke_test` with `ZED_PKG_TEST_TARGET`
   pointing at the installed package.

The whole workspace lives under your home directory at
`~/.zed-pkg/r2g/<org>-<name>-<uuid-v4>/` (registry + consumer + store). Unique
run directories prevent stale or concurrent state from masking a failure and
are left behind for inspection (pass `--clean`, or set `--r2g-root` to
relocate them). `zed test-local` is a backwards-compatible alias.

```sh
zed r2g                       # roundtrip on the host
zed r2g --docker              # ...inside a fresh debian:stable-slim container
zed r2g --docker --image node:22-slim   # pick an image with the runtime you need
```

With `--docker`, r2g installs in copy mode (self-contained, zero symlinks —
the same guarantee `--install-mode copy` gives OCI builds), bind-mounts the
mock consumer into a throwaway container, and runs the smoke test *there* —
proving the artifact works in a clean, host-independent environment (fresh
`$HOME`, distro libraries, none of your host toolchain leaking in). The
runtime is auto-detected (docker, then podman) or forced with `--runtime`.
If the smoke test passes here, it will pass for your users.

## VCS support

| VCS | Worktree check | Tag verification |
| --- | --- | --- |
| git | yes | yes |
| jj (colocated) | via git | via git |
| sapling (colocated) | via git | via git |
| hg | yes | yes |
| fossil | yes | not yet (`--skip-vcs-checks`) |
| pijul | not yet | not yet (`--skip-vcs-checks`) |

## Concurrency

`zed install` is safe to run from many processes at once (two terminals,
parallel CI runners). It takes an advisory `flock` on `~/.zed-pkg/locks/`
around store extraction (per-artifact) and the refs/lockfile writes
(per-install), so concurrent runs share one store without corrupting it, and
a crashed process never wedges the store (the OS drops the lock). See the
`concurrent_installs_share_the_store_safely` test.

## Platforms

Releases ship prebuilt `zed` binaries for macOS (Apple Silicon + Intel),
Linux (arm64 + x64, gnu and musl), and Windows via
[`.github/workflows/release.yml`](.github/workflows/release.yml) on every
`v*` tag. musl builds are static — ideal for distroless/scratch containers.

## Store layout

```
~/.zed-pkg/
  store/v1/<aa>/<sha256>/pkg/          extracted source artifacts (content-addressed, immutable)
  builds/v1/<platform>/<aa>/<sha256>/  per-platform build-hook outputs
  cache/<sha256>.tar.gz                downloaded archives
  locks/                               advisory flocks (per-artifact, per-install, per-build)
  auth/sessions.toml                   shared-auth + Supabase token pairs (0600)
  refs.json                            project -> artifact references (for prune/gc)
  credentials.toml                     legacy opaque registry tokens (0600)
```

## Hardening

Artifacts arrive over the network, so the client treats them as untrusted:

- **Digest-addressed everything.** Registry-returned `org`, `name`, and
  `sha256` are validated (slug / 64-char hex) before they touch a filesystem
  path — a hostile registry can't traverse out of the store or `zed_modules/`.
- **Extraction is a security boundary.** Tar/zip entries are screened for
  path traversal (`..`, absolute, prefix components), symlink/hardlink and
  non-regular entries are refused, per-entry size is bounded by the declared
  header (a lying header can't over-read), and total unpacked size and entry
  count are capped (`ZED_PKG_MAX_UNPACKED_BYTES`) against decompression bombs.
- **Bounded downloads.** Artifact fetches are size-capped
  (`ZED_PKG_MAX_ARTIFACT_BYTES`) and a registry-supplied `download_url` must
  be https (or loopback/http only when the registry itself is http).
- **No install-time code execution.** Installing a dependency never runs its
  scripts; `[build]` steps run only with explicit `--allow-build`.
- **Recoverable project mutations.** Install and uninstall stage old paths in
  UUID-v4 transaction directories and restore interrupted work before the
  next lifecycle operation.
- **Tokens at 0600 from creation**, with no write-then-chmod window.

## Development

Clone side by side with [zed-interfaces](https://github.com/zed-pkg/zed-interfaces)
(path dependency), then:

```sh
cargo test
```

## License

MIT