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
  for Docker build contexts, OCI layers, and read-only runtimes. First-class
  [project-owned CLI runtimes](docs/cli-tools.md) make Node.js and Python part
  of that same auditable workspace instead of an opaque base-image choice.

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
zed publish           # registry + git tag on GitHub + Release + GHCR (org Packages page)

# consume packages from a manifest
zed add acme/http-kit@^1
zed install
zed uninstall                          # remove files; keep the exact lock
zed install --frozen                   # restore the same artifacts

# first dependency install in a project with no .zpkg.toml
zed install acme/http-kit@^1            # creates a basic durable .zpkg.toml

# explicit one-shot/ephemeral install: keep the project manifestless
zed install acme/http-kit@^1 --do-not-write-new-manifest
zed find http

# opt into a confirmation at every mutating lifecycle step
zed install --interactive
zed r2g --docker --interactive
zed publish --interactive

# create a new directory and give it project-owned CLI runtimes
zed init project --org acme
cd project
zed install --cli nodejs
zed install --cli python3
export PATH="$PWD/.zed/tools/bin:$PATH"
```

Every authored package is `<org>/<name>`, declared in a `.zpkg.toml` manifest
at the repo root (TOML only). A dependency-bearing first install also creates a
deterministic local consumer manifest when one is missing, so direct dependency
intent survives the shell invocation. The explicit
`--do-not-write-new-manifest` flag preserves the older in-memory consumer path
for throwaway or lock-only workflows. See `zed init` output for the annotated
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

Validate package metadata before install or publication with `zed validate`.
The command is offline and read-only: it never fetches, prompts, authenticates,
recovers transactions, or accesses the package store. It applies the pinned
zed-interfaces runtime validators plus the checked-in manifest/lock schema
shape, so malformed TOML, unknown canonical fields, invalid slugs or
provenance, unsupported lock versions, and direct requirement drift fail
closed. The known additive `[[git-submodule]]` lock extension is validated by
its own typed contract rather than rejected as an unknown canonical field.

```sh
zed validate
zed validate --require-lock
zed validate --manifest path/to/.zpkg.toml --lock path/to/.zpkg.lock --json
```

Lockfile v1 does not encode dependency edges. Validation therefore proves that
every direct runtime/build requirement is present and satisfied when a lock is
available, but explicitly does not claim transitive completeness. Without
`--require-lock`, an absent lock is reported as a warning so manifest-only
package artifacts can still smoke-test their authored metadata.

## Artifact formats

Artifacts are `tar.gz` by default; `zip` is fully supported (both pack
deterministically and install through the store's magic-byte extraction). The
registry hosts both on S3/Cloudflare R2.

Native executable bundles use the stricter self-describing ZIP profile described
in [docs/binary-artifacts.md](docs/binary-artifacts.md). `zed-binary pack` and
`verify` keep `.zpkg.toml` beside the payload under `pkg/`; publish/download use
the legacy version route by default or the additive target-qualified route with
`--artifact-route qualified`.

## Commands

| Command | What it does |
| --- | --- |
| `zed validate [--manifest PATH] [--lock PATH] [--require-lock] [--json]` | Validate canonical package metadata offline and without mutation; direct lock coverage is checked, while v1 transitive completeness is explicitly not claimed |
| `zed init [PROJECT]` | Create the optional project directory and write its `.zpkg.toml` template; `zed init project` infers the package name `project` |
| `zed add <org>/<name>[@req]` | Add a dependency and install |
| `zed remove <org>/<name>` | Remove a dependency |
| `zed install [<org>/<name>[@req] ...]` (`zed i`) | Resolve, download once into the store, and install; package operands create a durable consumer manifest when one is missing |
| `zed install --cli <tool> [--cli <tool> ...]` | Resolve exact project-owned CLI runtimes into `.zed/environment.lock.toml` and copy their complete runtime roots below `.zed/tools`; built-ins are `nodejs` and `python3` |
| `zed install --frozen` | Install exactly what the manifest/lock pair pins; a manifestless lock-only restore additionally requires `--do-not-write-new-manifest` |
| `zed uninstall [<org>/<name> ...]` (`zed un`) | Transactionally remove all or selected materialized packages while retaining the manifest and lockfile for a frozen reinstall |
| `zed inspect --root ABSOLUTE_PATH [--format json]` | Fully offline, read-only manifest, lock, store, Git-submodule, mise, and Nix analysis for IDEs and automation |
| `zed env import mise [--config PATH] [--lock PATH] [--frozen] [--json]` | Import the supported project-local mise tool/lock subset as the shared normalized `EnvironmentPlan`; never loads parent/global config or executes hooks |
| `zed env verify mise [--config PATH] [--lock PATH] --frozen [--json]` | Fail closed on missing lock coverage, drift, malformed checksums, unsupported semantics, or non-portable frozen state and report the stable plan digest |
| `zed env export mise --plan PATH [--output .mise.toml] [--check|--write] [--json]` | Deterministically project a schema-v2 environment plan into conflict-safe project-local mise TOML without invoking mise or executing project code |
| `zed env export devbox [--plan PATH] [--output devbox.json] [--receipt PATH] [--json]` | Deterministically generate Devbox JSON and a Zed-owned receipt without invoking Devbox |
| `zed env export flox [--plan PATH] [--output .flox/env/manifest.toml] [--receipt PATH] [--json]` | Deterministically generate Flox manifest TOML and a Zed-owned receipt without invoking Flox |
| `zed env import asdf [--config .tool-versions] [--lock .zed/asdf.lock.toml] [--frozen] [--json]` | Import project-local asdf selections and optional immutable plugin/artifact provenance without invoking asdf or plugin code |
| `zed env verify asdf [--config .tool-versions] [--lock .zed/asdf.lock.toml] --frozen [--json]` | Verify exact asdf tool, plugin revision, artifact SHA-256, platform, and normalized plan identity without reading parent/global configuration |
| `zed task list\|info\|graph\|run ...` | Use the shared schema-v2 runtime to discover, inspect, graph, dry-run, execute, confirm, parallelize, and content-cache project tasks; `zed-task` remains a compatibility binary |
| `zed find <query>` | Search the registry |
| `zed pack` | Build the pruned, deterministic `tar.gz` artifact |
| `zed-binary pack\|verify\|publish\|download` | Build or transport a deterministic, self-describing native ZIP; target-qualified registry identity is opt-in and does not modify SemVer |
| `zed release plan [--json] [--channel <track>]` | Print the credential-free Zed, native-registry, and forge-package release set derived from `.zpkg.toml` |
| `zed release preflight` | Validate native manifests, then run fixed credential-free package preflight adapters |
| `zed oci plan <oci://registry/repository:version> [--target <name>] [--out <layout>] [--json]` | Derive exact OCI identities and optionally materialize a verified local image layout without credentials or network transport |
| `zed oci push <layout> <oci://registry/repository:version>` | Verify a local OCI layout, copy it through ORAS using one explicit authentication mode, and require the remote tag to resolve to the expected digest |
| `zed release publish [--channel <track>] [--dry-run]` | Upload each native route to its ecosystem registry over that registry's own HTTP API |
| `zed release versions [--target <name>]` | List the versions each native route's registry already serves |
| `zed publish` | Verify clean tree + matching VCS tag at HEAD, pack, upload to the registry, then mirror the tarball to GitHub Releases and GHCR |
| `zed r2g` (`zed test-local`) | Roundtrip-test your artifact through a private file registry by default, or through the configured Rust HTTP registry with explicit `--registry-mode server`; then install it into a mock consumer and run `publish.smoke_test`, optionally inside an OCI container (`--docker`) |
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
| `zed mirror list\|check\|bootstrap\|sync` | Inspect, probe, recover, or materialize the configured package mirrors |
| `zed key generate\|list\|show\|enroll` | Manage publisher signing keys and enroll their public identities |
| `zed self-update [--check] [--force]` | Replace the binary with the latest GitHub release for your platform |
| `zed completions bash\|zsh` | Generate shell completion from the same Clap model used by the executable |

### Static inspection protocol

`zed inspect --root ABSOLUTE_PATH --format json` is the supported process
boundary for VS Code, Eclipse, Xcode, Sublime Text, and other tools. Inspection
is always offline and is dispatched before configuration, credentials, or
transaction recovery can be loaded. It never writes a lock, resolves a package,
runs mise or Nix, or executes package code. It reports schema-versioned
diagnostics and structured `argv` recommendations whose mutation, network, and
code-execution properties are explicit.

Add this declaration when Zed is expected to consume the checkout's committed
`.gitmodules` metadata:

```toml
[interop.git]
consume_gitmodules = true
```

`zed overtake --git-submodules` writes the declaration automatically, and
later `zed add`/`zed remove` manifest rewrites preserve it. A `.gitmodules`
file without the declaration is reported as a compatibility warning; a
declaration with missing, indirect, or ambiguous Git metadata is an error.

Mise analysis reuses the frozen, project-local import contract without loading
user/global configuration or invoking mise. Nix analysis checks `flake.nix`
and the adjacent `flake.lock` as data without running `nix develop`.

See [Static inspection and editor interoperability](docs/static-inspection.md)
for the JSON compatibility and plugin safety contract.

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

`zed release publish` and `zed release versions` talk to each ecosystem
registry over its own HTTP API rather than through that language's package
manager, so a polyglot repository can publish to Hex, Hackage, or CPAN from a
runner that has no Elixir, GHC, or Perl installed. `--channel rc` resolves the
release-candidate track each host actually uses — an npm dist-tag, a PEP 440
suffix on PyPI, a separate candidate endpoint on Hackage — rather than assuming
one spelling works everywhere. `--dry-run` prints the exact requests, with
credentials redacted, and sends nothing.

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

The Zed tarball itself is a different GitHub Packages path: `zed publish`
pushes it to GHCR (`ghcr.io/{owner}/{repo}:{tag}`) so it appears on
`https://github.com/orgs/{owner}/packages` as a container package. GitHub
has no native Zed package type; OCI artifacts are the supported surface
until one exists. Opt out with `[package.artifacts] github_packages = false`.

### First install without `.zpkg.toml`

`zed install` accepts package specs in an existing repository or folder:

```sh
zed install flags-2-env/flags-2-env@^0.3
```

Zed first searches upward for a Zed manifest. Without one, it looks for the
nearest native project marker (`package.json`, `Cargo.toml`, `go.mod`,
`pyproject.toml`, and other supported ecosystems). When invoked at a repository
shell containing exactly one clear nested app such as `apps/web/package.json`,
that app becomes the install root. Ambiguous monorepos stay at the requested
root and use the safe universal `zed_modules/` layout rather than guessing.

By default, a dependency-bearing first install writes a deterministic basic
`.zpkg.toml` at that selected root. It records the requested packages as direct
dependencies, records supported inferred target/adapter values, writes no
timestamps or machine-specific paths, and then runs the ordinary resolver,
integrity checks, lockfile transaction, store, materializer, adapters, and
build-hook policy. A failed install removes the exact generated manifest rather
than leaving a half-adopted project.

The generated manifest uses a local `zed-local/<directory-name>` identity,
version `0.0.0`, a non-authoritative localhost repository URL, and the
`zed-generated-consumer` marker. It is immediately suitable for dependency
management but `zed publish` rejects it until a maintainer reviews the real
package identity/repository metadata and removes the marker. `--skip-vcs-checks`
does not bypass that guard.

Use the canonical escape hatch when the project must remain manifestless:

```sh
zed install flags-2-env/flags-2-env@^0.3 --do-not-write-new-manifest
```

This preserves the established in-memory consumer plan. The normal installer
can still write `.zpkg.lock`, `zed_modules/`, hoisted bins, and supported
ecosystem adapter outputs; only creation of a missing `.zpkg.toml` is
suppressed. The canonical environment equivalent is
`ZED_PKG_DO_NOT_WRITE_NEW_MANIFEST=1`.

`--allow-no-manifest`, `--skip-manifest`, and
`ZED_PKG_ALLOW_NO_MANIFEST=1` remain compatibility spellings for one migration
window and emit deprecation guidance. When a manifest already exists, the new
flag is an informational no-op and never changes a managed project into an
ephemeral one.

A generated consumer manifest may accept additional package operands, which
lets two concurrent first installs retain both direct dependencies under the
project-scoped manifest lock. Conflicting requirements fail without replacing
the file. A human-authored existing manifest keeps the stricter rule: use
`zed add` to persist a dependency; positional operands on `zed install` are
rejected rather than silently editing authored package metadata.

A lockfile alone does not identify which packages were direct versus
transitive. Therefore an explicit lock-only restoration is:

```sh
zed install --frozen --do-not-write-new-manifest
```

Without the flag, Zed fails instead of inventing a misleading manifest from the
whole locked graph.

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

When a dependency resolves to a workspace member, `zed install` normally
symlinks the member's source directory straight into `zed_modules/` and keeps
resolving its transitive deps. A member with install hooks or a build step is
prepared in a writable staging copy and copied into the consumer instead, so
its finalized files never point at an ephemeral staging directory. Members are
not pinned in `.zpkg.lock` (there is no published artifact).

### Native dependencies, install hooks, and builds

A package declares host prerequisites by package-manager name. Package specs
are data, not commands: Zed maps each supported manager to a fixed argv
invocation and never interpolates a package spec into a shell.

```toml
[native-dependencies]
apt = ["pkg-config", "libssl-dev"]
apk = ["pkgconf", "openssl-dev"]
brew = ["pkg-config", "openssl@3"]
nix = ["pkg-config", "openssl"]

[hooks]
pre-install = ["./scripts/pre-install.sh"]
post-install = ["./scripts/post-install.sh"]
```

Polyglot packages may append target-specific entries under
`[targets.<target>.native-dependencies]` and `[targets.<target>.hooks]`.
Package-level hooks run before target hooks in each phase. Zed selects one
manager supported by every package in the resolved graph, de-duplicates its
package list, and installs it once before opening the consumer-project
transaction. Override detection with `--native-manager <name>`.

Native package installation and lifecycle hooks are separate trust decisions:

```sh
zed install --allow-native-deps --allow-install-hooks
```

The equivalent environment variables are `ZED_PKG_ALLOW_NATIVE_DEPS=1` and
`ZED_PKG_ALLOW_INSTALL_HOOKS=1`. Outside a Nix build, the `nix` route uses a
content-addressed Zed-managed profile below `$ZED_PKG_HOME/native/nix/` and
adds its build paths only to package lifecycle commands; it never changes the
user's default Nix profile. Inside a Nix build, Zed never invokes a package
manager: put declared prerequisites in `nativeBuildInputs` / `buildInputs`,
expose a `nix` route in the manifest, and set
`ZED_PKG_NATIVE_DEPS_PROVIDED=1` after the derivation has supplied them.

A package with native code or a codegen step may additionally declare a
`[build]`:

```toml
[build]
command = "cargo build --release"
outputs = ["target/release/libfoo.so"]   # empty = keep the whole tree

[build-dependencies]                       # tools needed only during the build
"acme/cmake" = "^3.20"
```

The lifecycle order is native prerequisites → `pre-install` hooks → build →
`post-install` hooks → cache promotion → project materialization. Hooks and
builds run in an isolated staging copy—never inside the immutable source store
or consumer project—and results cache by source hash, platform, lifecycle
commands, selected target, and native route under `~/.zed-pkg/builds/`.
Because a build runs arbitrary author code, it remains independently opt-in:
pass `--allow-build` (or set `ZED_PKG_ALLOW_BUILD=1`). A consumer can patch or
replace a
dependency's build without waiting on upstream:

```toml
[overrides.build."acme/crypto"]
command = "make install CC=clang"
```

## Flags-2-env

Following the [flags-2-env](https://github.com/flags-2-env/flags-2-env)
convention, every flag can be set via a `ZED_PKG_*` environment variable. The
full mapping is declared, TOML-only, in
[`.cli-flags.toml`](.cli-flags.toml) — a `cargo test` asserts that file and the
actual CLI never drift, so it is always authoritative:

| Flag | Env var | Default |
| --- | --- | --- |
| `--registry` | `ZED_PKG_REGISTRY` | `https://registry.zpkg.net` |
| `--r2-public-base` | `ZED_PKG_R2_PUBLIC_BASE` | `https://cdn.zpkg.net` (Cloudflare → R2; independent of the registry origin) |
| `--r2-public-key` | `ZED_PKG_R2_PUBLIC_KEY` | optional hostname, `https://…`, or Cloudflare `pub-<id>` |
| `--source-fallback` | `ZED_PKG_SOURCE_FALLBACK` | on; retry public R2 and GitHub when the HTTP registry is down (`file://` and loopback stay hermetic) |
| (env only) | `ZED_PKG_SOURCE_FALLBACK_ALLOW_LOOPBACK` | off; test-org canaries that bind mocks to `127.0.0.1` must set this |
| `--home` | `ZED_PKG_HOME` | `~/.zed-pkg` |
| `--token` | `ZED_PKG_TOKEN` | saved credentials |
| `--auth-url` | `ZED_PKG_AUTH_URL` | `<registry>/shared-auth` |
| `--supabase-url` | `ZED_PKG_SUPABASE_URL` | optional Supabase project URL |
| `--supabase-key` | `ZED_PKG_SUPABASE_KEY` | optional public publishable/anon key |
| `--interactive` | `ZED_PKG_INTERACTIVE` | off; confirm each mutating lifecycle step in a real terminal |
| `--manifest` (validate) | `ZED_PKG_VALIDATE_MANIFEST` | `.zpkg.toml` |
| `--lock` (validate) | `ZED_PKG_VALIDATE_LOCK` | `.zpkg.lock` |
| `--require-lock` (validate) | `ZED_PKG_VALIDATE_REQUIRE_LOCK` | off |
| `--json` (validate) | `ZED_PKG_VALIDATE_JSON` | off |
| `--install-mode` | `ZED_PKG_INSTALL_MODE` | `symlink` |
| `--cli <tool>` (install) | `ZED_PKG_CLI` | none; repeat on the command line for multiple tools |
| `--cli-target` (install) | `ZED_PKG_CLI_TARGET` | detected GNU/Linux architecture |
| `--cli-install-mode` (install) | `ZED_PKG_CLI_INSTALL_MODE` | `copy` |
| `--adapter` | `ZED_PKG_ADAPTER` | `auto` — context-aware linking: `package.json` projects also get `node_modules/@org/name` links; `pom.xml`/`build.gradle` projects get a generated `.zed/classpath` of installed jars for `java -cp "$(cat .zed/classpath)"`; python site-packages planned |
| `--frozen` | `ZED_PKG_FROZEN` | off |
| `--allow-build` (install) | `ZED_PKG_ALLOW_BUILD` | off |
| `--allow-native-deps` | `ZED_PKG_ALLOW_NATIVE_DEPS` | off |
| `--allow-install-hooks` | `ZED_PKG_ALLOW_INSTALL_HOOKS` | off |
| `--native-manager <name>` | `ZED_PKG_NATIVE_MANAGER` | auto-detect one graph-compatible manager |
| `--do-not-write-new-manifest` (install) | `ZED_PKG_DO_NOT_WRITE_NEW_MANIFEST` | off; normal first installs create a basic durable `.zpkg.toml` |
| deprecated `--allow-no-manifest` / `--skip-manifest` | deprecated `ZED_PKG_ALLOW_NO_MANIFEST` | compatibility aliases for `--do-not-write-new-manifest` |
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
| `--registry-mode` (r2g) | `ZED_PKG_R2G_REGISTRY_MODE` | `isolated`; `server` intentionally persists the package version on the configured HTTP(S) registry |
| `--docker` (r2g) | `ZED_PKG_R2G_DOCKER` | off |
| `--image` (r2g) | `ZED_PKG_R2G_IMAGE` | `debian:stable-slim` |
| `--runtime` (r2g) | `ZED_PKG_R2G_RUNTIME` | auto (docker, then podman) |
| `--r2g-root` (r2g) | `ZED_PKG_R2G_ROOT` | `<home>/r2g` |
| `--clean` (r2g) | `ZED_PKG_R2G_CLEAN` | off |

`--registry file:///path` selects a directory-backed registry: hermetic CI,
air-gapped mirrors, and the default `zed r2g` mode all use it.

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

Symlinks into `$HOME/.zed-pkg` do not survive a `COPY --from=build` between
image stages. Project-owned CLI runtimes therefore default to copy mode: their
complete runtime roots, command links, and portable environment lock all live
below the workspace. The published builder image supports an intentionally
small, readable multi-stage recipe:

```dockerfile
FROM ghcr.io/zed-pkg/zed-oci:0.2.0 AS zed-builder
WORKDIR /workspace

RUN zed init project --org example
WORKDIR /workspace/project
RUN zed install --cli nodejs
RUN zed install --cli python3

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=zed-builder /workspace/project/ /app/
ENV PATH="/app/.zed/tools/bin:${PATH}"

# The copied workspace owns both runtimes; Zed and its store stay behind.
RUN node --version \
 && python3 --version \
 && ! command -v zed \
 && test ! -e /home/zed/.zed-pkg
```

`node`/`nodejs`, `npm`, `npx`, and `corepack` come from the locked Node.js
runtime. `python`/`python3`/`python3.14` and `pip`/`pip3`/`pip3.14` come from
the locked Python runtime. The built-in catalog currently targets glibc-based
x86_64 and arm64 Linux, so both the builder and the final image must provide a
GNU/Linux runtime. See [CLI tools](docs/cli-tools.md) for the lock, target, and
update contract.

Package dependencies use their existing explicit copy mode and can share the
same builder stage:

```dockerfile
FROM ghcr.io/zed-pkg/zed-oci:0.2.0 AS build
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
- `zed install --cli ...` writes a separate portable
  `.zed/environment.lock.toml` and defaults to project-owned copy mode.
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
2. **Publish** — to a private throwaway `file://` registry by default, or to
   the configured HTTP(S) server only with `--registry-mode server`.
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
zed r2g                       # safe, hermetic roundtrip on the host
zed r2g --docker              # ...inside a fresh debian:stable-slim container
zed r2g --docker --image node:22-slim   # pick an image with the runtime you need

# Certify a disposable Rust registry reached through a port-forward. Publishing
# is persistent from the registry's point of view: an identical retry reuses
# the immutable version, while changed bytes require a new version or a reset.
zed --registry http://127.0.0.1:48080 --token "$ZED_PKG_TOKEN" \
  r2g --registry-mode server --clean
```

Server mode is deliberately loud and explicit. It uses the ordinary HTTP
registry client for both upload and consumer install, including the configured
credential. The package version remains published after `--clean`; cleanup
only removes the local mock consumer and store. A byte-identical repeated run
reuses the immutable version. If the package bytes change without a version
bump, r2g fails before upload; publish a new version or reset the disposable
registry metadata and process-memory artifact store together. This is the mode
used to certify the bounded Rust process-memory backend on the AWS and Hetzner
Kubernetes deployment paths; the ordinary pre-publish developer loop remains
hermetic.

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
parallel CI runners). Store extraction and reference updates retain their
existing advisory locks. A dependency-bearing first install also takes a
project-scoped manifest lock under `~/.zed-pkg/locks/projects/`, keyed by the
canonical project path. Two simultaneous first installs therefore create one
valid manifest and merge distinct direct dependencies instead of losing one
caller's intent. Exact conflicting requirements fail rather than choosing a
winner, and the OS releases every lock if a process dies.

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
  locks/projects/<hash>.manifest.lock  project-scoped first-install serialization
  locks/                               advisory flocks (per-artifact, per-install, per-build)
  auth/sessions.toml                   shared-auth + Supabase token pairs (0600)
  refs.json                            project -> artifact references (for prune/gc)
  credentials.toml                     legacy opaque registry tokens (0600)
```

Verified binary downloads additionally receive a human-readable, source- and
target-qualified view under ~/.zpkg/downloads. The existing
~/.zed-pkg/store remains the content-addressed byte authority. The host view
uses Windows-safe typed folders such as
zed-org--acme/zed-project--payments/zed-package--tool/versions/1.2.3/zed/targets/aarch64-linux-android.
Projectless packages omit the project segment. Configure the root, delimiter,
source precedence, and project/package discovery indexes in
~/.zpkg/zpkg-config.toml; see
[docs/zpkg-config.toml.example](docs/zpkg-config.toml.example).

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
  be https (or loopback/the same http origin as an explicitly plaintext
  registry). Authenticated API requests never follow redirects.
- **No implicit install-time code execution or privilege use.** Native package
  installation, package lifecycle hooks, and builds require independent
  explicit consent. Native specs use fixed argv templates; hooks/builds run in
  disposable writable staging copies, never in the source store or consumer
  project.
- **Generated identities fail closed.** A first-install consumer manifest
  cannot be published until its inferred local package identity is reviewed
  and the generated marker is removed.
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
