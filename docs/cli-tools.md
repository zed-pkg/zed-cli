# Project-owned CLI runtimes

`zed install --cli` makes language runtimes part of the project rather than an
implicit property of a developer machine or container base image. Zed resolves
an exact built-in catalog entry, records every supported platform artifact in a
portable lock, verifies the downloaded bytes, and copies the complete runtime
below `.zed/tools`.

The result is designed for OCI multi-stage builds: copy the project into the
final stage and leave the `zed` executable, its download cache, and its global
store behind.

## Project creation and installation

`zed init` accepts an optional project directory. Relative directories are
created below the current working directory, and the final directory name is
used as the default package name.

```console
zed init project --org example
cd project

zed install --cli nodejs
zed install --cli python3
export PATH="$PWD/.zed/tools/bin:$PATH"
```

The options are repeatable, so a fresh project can install both runtimes in one
transaction:

```console
zed install --cli nodejs --cli python3
```

Separate invocations are incremental: the second command extends the existing
environment lock and atomically replaces the active target profile with both
tools. Package operands and CLI tools deliberately cannot be mixed in one
invocation; use two commands when a project needs both:

```console
zed install --frozen --install-mode copy
zed install --frozen --cli nodejs --cli python3
```

## Built-in catalog

The initial catalog is deliberately narrow and reviewable:

| Requested name | Locked version | Accepted version aliases | Commands |
| --- | --- | --- | --- |
| `node` or `nodejs` | Node.js `24.19.0` | `24`, `24.19`, `lts`, `latest-lts`, `latest` | `node`, `nodejs`, `npm`, `npx`, `corepack` |
| `python` or `python3` | CPython `3.14.7` from python-build-standalone build `20260807` | `3`, `3.14`, `latest` | `python`, `python3`, `python3.14`, `pip`, `pip3`, `pip3.14` |

An exact version can be requested with `@`, for example
`zed install --cli nodejs@24.19.0`. An unsupported requirement fails instead
of silently selecting a nearby version.

Both `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu` artifacts are
written into the same lock. This lets one committed lock drive a
multi-architecture OCI publication without regenerating platform-specific
metadata. Native invocation detects the Linux architecture and verifies that a
glibc loader exists. `--cli-target TARGET` is available to an OCI build system
that explicitly selects a target.

## Lock and project layout

The portable lock is `.zed/environment.lock.toml`. `zed init` ignores generated
profiles while keeping that lock eligible for source control:

```gitignore
.zed/*
!.zed/environment.lock.toml
```

The installed layout is:

```text
.zed/
├── environment.lock.toml
└── tools/
    ├── bin -> v1/<target>/bin
    └── v1/<target>/
        ├── bin/
        │   └── <command> -> ../roots/<tool>/<locked executable>
        ├── profile.json
        └── roots/
            ├── nodejs/
            └── python3/
```

All command links are relative and every target is inside the copied project.
Deleting `$ZED_PKG_HOME`, unmounting a BuildKit cache, or omitting the builder's
home from a later OCI stage cannot break the profile.

`--cli-install-mode copy` is the default and the only mode accepted by the
built-in upstream-runtime catalog. The lower-level `zed-tool` replay command
continues to support store-backed symlink profiles for authenticated Zed
package artifacts.

## Frozen and update behavior

Without `--frozen`, requested tools are resolved against the catalog and merged
into the portable lock. With `--frozen`, every requested tool and requirement
must already be represented by the lock; Zed selects only the exact target
variant and rejects drift.

Catalog updates are source changes, not mutable server-side aliases. Updating a
runtime requires reviewing its upstream URL, byte size, SHA-256, install root,
and exposed executables, then releasing a new Zed version. Existing environment
locks retain their exact artifact identities.

## OCI example

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

RUN node --version \
 && python3 --version \
 && ! command -v zed \
 && test ! -e /home/zed/.zed-pkg
```

The initial catalog is GNU/Linux-only, so use a glibc-based builder and final
stage. The final stage may be non-root; the copied files do not need a Zed home
or network access.

## Verification boundaries

Before activation, Zed requires:

1. an HTTPS source URL;
2. the exact locked compressed size and SHA-256;
3. a gzip tar archive with at most 200,000 entries and 2 GiB unpacked data;
4. only relative, traversal-free archive paths and relative symlinks that stay
   inside the authenticated root;
5. one exact variant for the selected target;
6. regular, executable locked command files with no command-name collisions;
7. an atomically replaceable, Zed-owned profile directory.

Downloads first enter the content-addressed Zed cache. Raw upstream runtime
archives are extracted under a separate digest-keyed tool store and copied into
the staged project profile. A failed download, extraction, validation, or
profile commit leaves the prior active profile in place.

Arbitrary catalog plugins, arbitrary URLs on the command line, non-Linux
runtimes, and automatic floating-version updates are intentionally outside this
initial contract.
