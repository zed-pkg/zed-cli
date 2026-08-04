# Git submodule interoperability

Zed and Git submodules can coexist in one repository. Git remains a supported
checkout transport, while Zed can become the authority for dependency identity,
workspace resolution, integrity, and frozen replay.

## Cooperative install mode

Submodule handling is opt-in and defaults to off. Enable it for one install:

```sh
zed install --git-submodules
```

The equivalent environment setting is:

```sh
ZED_PKG_GIT_SUBMODULES=1 zed install
```

Boolean values accept `true`/`false`, `1`/`0`, `yes`/`no`, and `on`/`off`.
An explicit CLI value can disable an inherited environment setting:

```sh
zed install --git-submodules=false
```

When enabled, Zed runs the equivalent of:

```sh
git submodule sync --recursive
git submodule update --init --recursive --checkout
```

The explicit `--checkout` strategy prevents repository configuration from
selecting a custom `submodule.<name>.update = !command` hook. With no
`.gitmodules` at or above the invocation directory, the operation is a safe
no-op and ordinary Zed installation continues.

A fresh-clone frozen restore can therefore use one command:

```sh
git clone <superproject>
cd <superproject>
zed install --git-submodules --frozen
```

## Overtaking submodules

Use takeover when the checked-out submodules are Zed packages and the root
project should declare them as part of its Zed graph:

```sh
zed overtake --git-submodules
```

Takeover performs these steps:

1. synchronizes and initializes all configured submodules recursively;
2. requires each top-level submodule to contain a valid `.zpkg.toml`;
3. verifies that `.gitmodules` and each gitlink are committed at superproject
   `HEAD`;
4. requires each submodule checkout to match its committed gitlink and have no
   tracked or untracked changes;
5. adds each package path to `[workspace].members`;
6. adds an exact direct requirement under `[dependencies]`;
7. runs the normal Zed solver and transactional installer; and
8. records immutable Git provenance in `.zpkg.lock`.

The authority migration is failure-safe. If resolution or materialization fails
before the ordinary install transaction commits, Zed restores the exact prior
root-manifest bytes, or removes the generated manifest when none existed. It
also refuses to overwrite a manifest changed by another writer while takeover
was running. If package installation commits but the additive Git-lock finalizer
alone fails, Zed retains the adopted manifest so it remains aligned with
installed state and reports the reconciliation failure explicitly.

For example, a submodule at `vendor/client` declaring `acme/client@1.2.3`
produces manifest intent equivalent to:

```toml
[workspace]
members = ["vendor/client"]

[dependencies]
"acme/client" = "=1.2.3"
```

If the superproject has no `.zpkg.toml`, takeover creates the same deterministic,
non-publishable local consumer manifest used by a first `zed install` with
package operands.

Package identity comes from each submodule's own `.zpkg.toml`; Zed does not
guess an organization or package name from a Git URL. A repository that also
contains non-Zed submodules can continue using cooperative install mode. To
overtake one, first add a valid package manifest to that submodule repository
and commit it.

## Lock authority

Registry packages continue to use ordinary `[[package]]` entries. Adopted
submodules add typed, additive records:

```toml
[[git-submodule]]
name = "client"
path = "vendor/client"
package = "acme/client"
version = "1.2.3"
url = "https://github.com/acme/client.git"
commit = "0123456789abcdef0123456789abcdef01234567"
sha256 = "..."
size = 12345
format = "tar.gz"
```

The SHA-256 and size describe a deterministic package artifact produced from
`git archive HEAD` and Zed's normal pruning rules. The commit pins the complete
Git tree; nested submodule gitlinks are therefore transitively pinned by that
commit and are also checked for initialized/exact status.

The extension is additive: the canonical lock parser can still read the normal
lock graph, while this CLI verifies the Git records before any install mutation.
A frozen install fails on path, package, version, URL, commit, artifact digest,
size, format, name, or configured-branch drift and never rewrites the lockfile.
A non-frozen install recomputes the records after normal resolution.

## Reversibility and ownership

Takeover deliberately retains `.gitmodules` and Git's gitlinks. Existing Git
clone/update workflows therefore remain valid, and Zed can use them to restore
workspace sources before resolving the package graph. Zed becomes authoritative
for dependency declarations and lock integrity; Git remains a compatible source
transport.

Removing an adopted package from the active Zed dependency graph removes its
Git lock record on the next non-frozen install. It does not delete the submodule
or rewrite `.gitmodules`; those remain explicit Git operations.

## Packing and publishing submodule source

`zed pack` and `zed publish` fail closed when a configured Git submodule can
contribute files to an artifact. Every included submodule must:

- be initialized;
- resolve inside the superproject;
- match the gitlink committed at superproject `HEAD`;
- have no tracked or untracked changes; and
- have no uninitialized, conflicted, dirty, or commit-drifted nested submodule.

The CLI reports `zed install --git-submodules` as the recovery command for an
uninitialized or drifted checkout. A submodule excluded from every artifact by
`publish.exclude` or `.zedignore` does not need to be initialized. Polyglot
packages apply this test independently to every target source root, including a
target located inside a submodule.

VCS control data is never package payload. Pack and publish add non-persistent
exclusions for root and nested `.git`, `.hg`, and `.svn` control paths, including
Git worktree/submodule `.git` pointer files and `.gitmodules`. These rules do not
rewrite the authored `.zpkg.toml`; they harden only the active packaging
operation.

This boundary prevents a fresh clone with an uninitialized gitlink from
producing a valid-looking but incomplete archive, while still allowing a fully
materialized submodule to be embedded as ordinary runtime source.

## Safety constraints

Zed refuses takeover or lock refresh when:

- `.gitmodules` has uncommitted changes;
- a configured path escapes the project or targets `.git`/Zed recovery state;
- a workspace member resolves outside the superproject;
- two submodules declare the same package identity or path;
- the checkout differs from the gitlink committed at superproject `HEAD`;
- the submodule or any nested submodule is dirty, uninitialized, conflicted, or
  checked out at a different commit; or
- a package manifest is invalid.

These checks keep `zed install --frozen` reproducible without making Git and Zed
mutually exclusive.
