# Git submodule interoperability

Zed and Git submodules can coexist in one repository. Git remains a supported
checkout transport, while Zed can become the authority for dependency identity,
workspace resolution, integrity, and frozen replay.

## Cooperative install mode

Submodule handling is opt-in and defaults to off. The compatibility switch is
global, so it can appear before or after the command:

```sh
zed --git-submodules install
zed install --git-submodules
```

The equivalent environment setting is:

```sh
ZED_PKG_GIT_SUBMODULES=1 zed install
```

Repositories can make that interoperability intent durable in `.zpkg.toml`:

```toml
[interop.git]
consume_gitmodules = true
```

`zed install` then synchronizes submodules without requiring a per-invocation
flag. `zed inspect --root ABSOLUTE_PATH --format json` reports a `.gitmodules`
file without this declaration as a cross-tool compatibility warning. The
declaration must be a boolean and is preserved when dependency commands rewrite
the typed manifest.

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

All configured Git submodules are synchronized and initialized in this mode,
including submodules that are not Zed packages. Only adopted Zed packages
participate in `.zpkg.toml` and `.zpkg.lock` authority. Before Zed parses
`.gitmodules`, its worktree entry must be a regular file and any indexed entry
must be a stage-zero regular Git blob; symlinked, conflicted, directory, or other
indirect metadata fails closed.

## Overtaking submodules

Use takeover when checked-out submodules contain Zed packages and the root
project should declare those packages as part of its Zed graph:

```sh
zed overtake --git-submodules
```

Takeover performs these steps:

1. synchronizes and initializes all configured submodules recursively;
2. discovers top-level submodules containing `.zpkg.toml` and leaves ordinary
   non-Zed submodules under Git authority;
3. requires every discovered `.zpkg.toml` to be a regular file and valid;
4. verifies that `.gitmodules` and each adopted gitlink are committed at
   superproject `HEAD`;
5. requires each adopted checkout to match its committed gitlink and have no
   tracked or untracked changes;
6. records `[interop.git].consume_gitmodules = true` in the root manifest;
7. adds each adopted package path to `[workspace].members`;
8. adds an exact direct requirement under `[dependencies]`;
9. runs the normal Zed solver and transactional installer; and
10. records immutable Git provenance in `.zpkg.lock`.

This makes takeover incremental in a mixed repository. For example, a project
may keep a documentation theme or large fixture repository as an ordinary Git
submodule while adopting only its Zed SDK packages. Missing `.zpkg.toml` means
“leave this submodule Git-managed”; a present but malformed or non-regular
`.zpkg.toml` is an error rather than something Zed silently ignores. When none
of the configured submodules are Zed packages, takeover still performs the
requested cooperative Git synchronization but leaves `.zpkg.toml`, `.zpkg.lock`,
and materialized Zed state unchanged before returning an actionable error.

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

A neighboring submodule such as `vendor/docs-theme` with no `.zpkg.toml`
remains in `.gitmodules` and is still initialized by
`zed install --git-submodules`, but it is not added to the Zed workspace or
lockfile.

If the superproject has no `.zpkg.toml`, takeover creates the same deterministic,
non-publishable local consumer manifest used by a first `zed install` with
package operands.

Package identity comes from each adopted submodule's own `.zpkg.toml`; Zed does
not guess an organization or package name from a Git URL. To adopt a currently
Git-only submodule later, add and commit a valid package manifest in that
submodule and run takeover again.

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
for adopted dependency declarations and lock integrity; Git remains the source
transport for every submodule and the sole authority for non-Zed submodules.

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
`publish.exclude` or `.zedignore` does not need to be initialized. Zed treats an
uninitialized subtree as conclusively excluded only for a canonical, literal
`prefix/**` rule; it does not normalize leading `./`, leading `/`, alternate
path separators, whitespace, or wildcard-bearing prefixes into that exception.
All other patterns fail closed and require the checkout. Polyglot packages apply
this test independently to every target source root, including a target located
inside a submodule.

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

- `.gitmodules` is indirect, non-regular, conflicted in the index, or has
  uncommitted changes;
- a configured path escapes the project or targets `.git`/Zed recovery state;
- a workspace member resolves outside the superproject;
- two adopted submodules declare the same package identity or path;
- an adopted checkout differs from the gitlink committed at superproject
  `HEAD`;
- an adopted submodule or any of its nested submodules is dirty, uninitialized,
  conflicted, or checked out at a different commit; or
- a discovered package manifest is invalid or is not a regular file.

These checks keep `zed install --frozen` reproducible without making Git and Zed
mutually exclusive.
