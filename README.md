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
- **Container-first.** A copy install mode and cache-mount patterns designed
  for OCI images and multi-stage Docker builds.

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
zed test-local        # r2g-style: consume your own artifact before shipping
zed publish

# consume packages
zed add acme/http-kit@^1
zed install
zed find http
```

Every package is `<org>/<name>`, declared in a `.zpkg.toml` manifest at the
repo root (TOML only). See `zed init` output for the annotated template.

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
| `zed install` (`zed i`) | Resolve, download once into the store, symlink into `zed_modules/` |
| `zed install --frozen` | Install exactly what `.zpkg.lock` pins (CI/containers) |
| `zed find <query>` | Search the registry |
| `zed pack` | Build the pruned, deterministic `tar.gz` artifact |
| `zed publish` | Verify clean tree + matching VCS tag at HEAD, pack, upload |
| `zed test-local` (`zed r2g`) | Install your own artifact into a throwaway consumer and run `publish.smoke_test` |
| `zed run <bin> [args]` | Run an executable a dependency exposes via `[bin]`, with `zed_modules/.bin` on `PATH` (npx-style, no global pollution) |
| `zed yank <org>/<name>@<version> [--undo]` | Hide a version from fresh resolution (existing lockfiles keep working) |
| `zed login` | Save a registry token to `~/.zed-pkg/credentials.toml` |
| `zed org claim <slug>` | Claim a namespace |
| `zed store status\|path\|prune` | Inspect the store or prune unreferenced entries |
| `zed gc [--max-age-days N]` | Age-aware collection: drop entries no live project references and unused past the cutoff, plus stale downloads |
| `zed cache clean` | Drop cached downloads |
| `zed self-update [--check]` | Replace the binary with the latest GitHub release for your platform |

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
store — and results cache per `(sha256, platform)` under `~/.zed-pkg/builds/`.
Because a build runs arbitrary author code, it is opt-in: pass `--allow-build`
(or set `ZED_PKG_ALLOW_BUILD=1`). A consumer can patch or replace a
dependency's build without waiting on upstream:

```toml
[overrides.build."acme/crypto"]
command = "make install CC=clang"
```

## Flags-2-env

Following the [flags-2-env](https://github.com/oresoftware/flags-2-env)
convention, every flag can be set via a `ZED_PKG_*` environment variable:

| Flag | Env var | Default |
| --- | --- | --- |
| `--registry` | `ZED_PKG_REGISTRY` | `https://registry.zpkg.tech` |
| `--home` | `ZED_PKG_HOME` | `~/.zed-pkg` |
| `--token` | `ZED_PKG_TOKEN` | saved credentials |
| `--install-mode` | `ZED_PKG_INSTALL_MODE` | `symlink` |
| `--adapter` | `ZED_PKG_ADAPTER` | `auto` — context-aware linking: `package.json` projects also get `node_modules/@org/name` links; `pom.xml`/`build.gradle` projects get a generated `.zed/classpath` of installed jars for `java -cp "$(cat .zed/classpath)"`; python site-packages planned |
| `--frozen` | `ZED_PKG_FROZEN` | off |
| `--allow-dirty` | `ZED_PKG_ALLOW_DIRTY` | off |
| `--skip-vcs-checks` | `ZED_PKG_SKIP_VCS_CHECKS` | off |
| `--out` (pack) | `ZED_PKG_PACK_OUT` | `.zed/pack` |
| `--org` (init) | `ZED_PKG_ORG` | - |

`--registry file:///path` selects a directory-backed registry: hermetic CI,
air-gapped mirrors, and `zed test-local` all use it.

## Containers & OCI

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

## test-local (r2g)

Inspired by [r2g](https://github.com/oresoftware/r2g): the failure mode it
kills is "works in my repo, breaks when installed." `zed test-local` packs
your artifact, publishes it to a throwaway `file://` registry, installs it
into a throwaway consumer project with a throwaway store, then runs your
`publish.smoke_test` with `ZED_PKG_TEST_TARGET` pointing at the installed
package. If the smoke test passes there, it will pass for your users.

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
  store/v1/<aa>/<sha256>/pkg/   extracted artifacts (content-addressed)
  cache/<sha256>.tar.gz         downloaded archives
  refs.json                     project -> artifact references (for prune)
  credentials.toml              registry tokens (0600)
```

## Development

Clone side by side with [zed-interfaces](https://github.com/zed-pkg/zed-interfaces)
(path dependency), then:

```sh
cargo test
```

## License

MIT
