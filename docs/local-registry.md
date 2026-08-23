# The local project registry

`zed install` normally asks the configured registry where a package lives.
That is the wrong answer in three ordinary situations: the registry is down
(including while it is being built), the machine is offline or air-gapped, and
the dependency you need is a checkout sitting next to the project you are
editing.

The local project registry is the answer for all three. A directory containing
a `.zpkg.toml` can be *registered* into a shared index under the Zed home
directory, and installs then resolve that package from the filesystem — no
HTTP, no store round trip, and with symlink install mode, no copy either.

```sh
cd ~/src/widget && zed local register     # this checkout provides acme/widget
cd ~/src/app    && zed install            # acme/widget resolves to ~/src/widget
```

## Identity is the path, not the package name

Registrations are keyed by **canonical filesystem path**. One package may
legitimately exist in several checkouts at once — a release clone, a feature
worktree, a bisect tree — and a registry keyed on `org/name` alone would have
to silently pick one of them. So:

- registering the same path twice refreshes that entry instead of adding one;
- registering a second path for the same package adds a second entry;
- the path is canonicalized first, so a symlinked spelling of a directory and
  the directory itself can never both be registered, and an entry keeps
  meaning the same tree after a convenience symlink is repointed.

When several registrations provide the same package, selection is: highest
`--priority`, then highest version, then path order. A tie on **both** priority
and version between two different paths is a hard error, not a coin flip:

```
error: local registry cannot choose between 2 registrations of acme/widget@1.2.0
       at equal priority:
  /Users/dev/src/widget
  /Users/dev/src/widget-experiment
Break the tie with `zed local register <path> --priority N` or
`zed local unregister <path>`.
```

## Resolution is live; the snapshot is only for reporting

An entry records the package identity and version observed at registration
time. That snapshot is used by `zed local list` and to report drift. It is
**not** used to resolve: every install re-reads the manifest on disk, because
the point of a local registration is that the source is alive.

An entry is skipped, with a warning naming it, when the directory is gone, the
manifest stopped parsing, or the manifest now declares a different package.
`zed local prune` drops those. A registration you disabled on purpose is never
pruned — being shelved is not being broken.

## How much authority registrations have

`--local-registry` (or `ZED_PKG_LOCAL_REGISTRY`) picks the mode:

| Mode | Behavior |
| --- | --- |
| `off` | Registrations are ignored; every dependency comes from the remote registry. |
| `auto` (default) | Registrations satisfy ordinary installs before the network is consulted. `--frozen` installs ignore them. |
| `prefer` | As `auto`, and registrations may also satisfy `--frozen` installs. |
| `only` | Registrations are the only source. A dependency with no healthy local entry is an error, so the install cannot reach the network at all. |

`auto` is the default because a registration is never accidental: it exists
only because someone ran `zed local register` on that exact directory. Every
locally resolved dependency is still announced on stdout:

```
local acme/widget@1.2.0 -> /Users/dev/src/widget
```

### Why `--frozen` opts out by default

A frozen install replays an exact lockfile so two machines produce the same
tree. The local registry is machine-global ambient state; letting it silently
override a pin would make "frozen" mean something different on every laptop.
`prefer` and `only` say *use my machine's registrations anyway*, and the
resulting install is reproducible only with respect to this machine.

Requirements satisfied from live source have no immutable pin, so they are
exempt from the frozen "must be in `.zpkg.lock`" check — exactly as workspace
members already are.

## Precedence against workspace members

Workspace members win. They are declared by the root manifest of the tree being
installed, so they are part of the project; a registration is external state
someone configured on this machine. The order is:

1. workspace member (`[workspace] members`),
2. local registration (subject to the mode above),
3. the configured registry.

A dependency on the project's own package identity is never source-linked, in
either mechanism: it is a deliberate request for the published artifact. See
[install-resolution-hardening.md](install-resolution-hardening.md).

## Materialization and symlinks

Local registrations are materialized through the same source-link path as
workspace members, so install modes behave identically:

- **symlink mode** writes an absolute canonical directory symlink from
  `zed_modules/<org>/<name>` into the checkout. Edits in the checkout are
  visible to consumers immediately, with no reinstall.
- **copy mode** produces a standalone, symlink-free tree, using the guarded
  copier that rejects escaping symlinks, cycles, and special files.

A registration is refused at install time if it overlaps the project being
installed into, in either direction — linking a parent or a child of the
consumer would put `zed_modules/<org>/<name>` inside its own link target.
Transitive dependencies of a linked project are read from that checkout's
manifest and resolved the same way, so a whole local graph links at once.

## Hardening

Everything the index can influence reaches a filesystem path, so all of it is
validated on the way in and again on the way out.

Registration refuses:

- a path that does not exist, is not a directory, or has no `.zpkg.toml`;
- a `.zpkg.toml` that is a symlink — the identity of a tree must be decided
  inside that tree;
- a manifest that does not parse or does not validate;
- a path that is not valid UTF-8 (the index is JSON);
- any path inside the Zed home directory, or containing it. The store holds
  extracted *artifacts*; registering one would feed materialized output back in
  as source. The comparison uses the resolved home, so a symlinked
  `ZED_PKG_HOME` cannot slip past it.

The index itself is bounded and fail-closed:

- an unknown `schema` value is rejected rather than best-effort parsed;
- the file must be a regular, non-symlink file of at most **8 MiB**;
- at most **4096** entries, each with a slug-valid identity, a non-empty
  version, an absolute path, and no duplicate paths;
- every read-modify-write happens under an exclusive `zed-lock` guard beside
  the index, so two concurrent `zed local register` runs cannot lose one
  another's entry;
- writes are staged in the same directory and renamed into place, at mode
  `0600` under a `0700` directory.

`zed local scan` is bounded too: at most 32 levels and 200,000 directories, it
follows no symlinks, and it never descends into dependency, build, or VCS trees
(`node_modules`, `zed_modules`, `target`, `vendor`, `.git`, `.zed`, …) where a
manifest is materialized output rather than a checkout. Nested projects *are*
registered, so a workspace root and its members all land in the index.

## Commands

| Command | Purpose |
| --- | --- |
| `zed local register [PATH] [--priority N] [--disabled]` | Register or refresh a project directory. |
| `zed local unregister <SELECTOR> [--all]` | Forget a registration. |
| `zed local list [--json]` | Every registration with its current health. |
| `zed local enable\|disable <SELECTOR> [--all]` | Shelve a registration without losing its priority. |
| `zed local prune [--dry-run]` | Drop registrations that no longer resolve. |
| `zed local scan [PATH] [--max-depth N] [--priority N] [--dry-run]` | Discover and register a whole tree. |
| `zed local resolve <org/name> [--require REQ] [--json]` | Show what would be selected, and what was skipped. |
| `zed local path` | Print the index file location. |

A `SELECTOR` is an existing directory, an `org/name` key, or the 16-character
entry id from `zed local list --json`. A key selector that matches several
registrations requires `--all`, so `zed local unregister acme/widget` can never
quietly drop one you did not have in mind.

`ZED_PKG_LOCAL_REGISTRY_FILE` relocates the index wholesale, which is how
hermetic tests and throwaway sandboxes avoid touching real registrations.

## Verification

```sh
cargo test --locked --lib local_registry::tests -- --nocapture
cargo test --locked --test local_registry_cli -- --nocapture
cargo clippy --locked --all-targets -- -D warnings
```

The CLI test points the registry at an empty `file://` directory, so any
install that succeeds there succeeded because the local registry satisfied it.
