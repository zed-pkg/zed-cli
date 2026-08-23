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
| `zed local doctor [--json]` | Explain this machine's whole view: index, container detection, path mapping, link policy, and every entry's volume. |

A `SELECTOR` is an existing directory, an `org/name` key, or the 16-character
entry id from `zed local list --json`. A key selector that matches several
registrations requires `--all`, so `zed local unregister acme/widget` can never
quietly drop one you did not have in mind.

`ZED_PKG_LOCAL_REGISTRY_FILE` relocates the index wholesale, which is how
hermetic tests and throwaway sandboxes avoid touching real registrations.

## External and virtual disks

A checkout on an external SSD, a mounted disk image, or a network share is
registered like any other, and the volume it lives on is recorded with it:

| Volume | Recognized by |
| --- | --- |
| `removable` | `/Volumes/*` (macOS), `/media/*`, `/mnt/*`, `/run/media/*/*` |
| `network` | NFS, SMB/CIFS, sshfs, WebDAV |
| `container-mount` | virtiofs, 9p, grpcfuse, or any non-system bind mount seen from inside a container |
| `fixed` | everything else |

On Linux this comes from `/proc/self/mountinfo`; elsewhere — macOS in
particular — the volume root is recovered from the conventional layout, so an
ejected disk is still recognized as *a disk that is not here* rather than as a
directory that was deleted.

That distinction is the point of a separate `unavailable` state:

* `unavailable` — the volume is not mounted. `zed local prune` keeps the entry,
  because an external drive that happened not to be attached this afternoon is
  not a mistake to clean up.
* `stale: directory is gone` — the volume is mounted and the directory is not
  there. That registration really is broken, and prune drops it.

Neither state fails an install. A dependency whose local entry is unusable
falls through to the configured registry, with the reason printed, unless
`--local-registry=only` forbids the network.

### Removable media is copied, not linked

`zed_modules/<org>/<name>` is a symlink into the checkout on ordinary media.
That is wrong for media that can go away: the link dangles the moment the disk
is ejected or the container exits, and the breakage surfaces far from the
install that caused it. So a checkout on `removable`, `network`, or
`container-mount` storage is **copied** even in symlink install mode.

The exception is automatic: when the consuming project lives on the *same*
volume as the checkout, linking is allowed again — nothing outlives that volume
either way, so the link costs nothing.

Overrides, from most to least specific:

* `zed local register <path> --link symlink|copy|auto` records a per-entry
  preference;
* `--local-link-policy symlink|copy` (`ZED_PKG_LOCAL_LINK_POLICY`) is a
  process-wide operator decision and wins over the per-entry preference;
* `--local-ephemeral` (`ZED_PKG_LOCAL_REGISTRY_EPHEMERAL`) copies everything.

## Docker: `docker run` with a shared volume

Two things differ inside a container: the index lives somewhere else, and the
same bytes have a different absolute path.

```console
$ docker run --rm \
    -v "$HOME/codes:/work" \
    -v "$HOME/.zed-pkg/local-registry:/zed-local" \
    -e ZED_PKG_LOCAL_REGISTRY_FILE=/zed-local/index.json \
    -e ZED_PKG_LOCAL_REGISTRY_PATH_MAP="$HOME/codes=/work" \
    my-image zed install
```

`ZED_PKG_LOCAL_REGISTRY_FILE` points at the index that came in through a volume,
so the host's registrations are visible. `ZED_PKG_LOCAL_REGISTRY_PATH_MAP`
rewrites host paths into container paths: rules are `from=to` separated by
commas, the longest matching source prefix wins (so a nested rule refines a
broader one regardless of declaration order), both sides must be absolute, and
whichever side exists on this machine is canonicalized — a mount described
through a symlink (`/tmp` on macOS is really `/private/tmp`) still matches.

The map applies in both directions. A `zed local register` performed *inside*
the container writes the **host** path into the index, so one shared index stays
meaningful on both sides of the boundary.

Without the map, host-shaped entries simply do not resolve in the container: the
install says which path it looked for and falls back to the registry, which is
exactly the message that tells you to add the mapping.

Entries reached through a bind mount classify as `container-mount` and are
therefore copied, not linked — a symlink to `/work/...` inside `zed_modules/`
means nothing once the container exits.

## Docker: `docker build`

A build step has no volumes in the ordinary sense. Sources arrive through
BuildKit mounts that exist for one `RUN` and are absent from the resulting
image, so **nothing may be symlinked**:

```dockerfile
# syntax=docker/dockerfile:1.7
FROM ghcr.io/zed-pkg/zed-oci:0.2.0 AS build
WORKDIR /app
COPY .zpkg.toml .zpkg.lock ./

RUN --mount=type=bind,source=.,target=/src,ro \
    --mount=type=bind,source=.zed-local,target=/zed-local,ro \
    ZED_PKG_LOCAL_REGISTRY_FILE=/zed-local/index.json \
    ZED_PKG_LOCAL_REGISTRY_PATH_MAP="/home/alex/codes=/src" \
    ZED_PKG_LOCAL_REGISTRY_EPHEMERAL=1 \
    zed install
```

`ZED_PKG_LOCAL_REGISTRY_EPHEMERAL=1` declares that every registered checkout
lives on media that will not outlive this process, so all of them are copied and
the layer is self-contained. `--install-mode copy` remains the blunter
instrument that also detaches registry packages from the global store; the two
compose. If you would rather not rely on classification at all,
`--local-link-policy copy` is the explicit form and needs no BuildKit-specific
reasoning.

## Diagnosing a surprise

```console
$ zed local doctor
index          /Users/alex/.zed-pkg/local-registry/index.json
container      no
link policy    auto
ephemeral      no
path map       (none)
entries        2
  ok        acme/widget  /Users/alex/src/widget  [fixed]
  unusable  acme/proto   /Volumes/Scratch/proto  [removable]
            unavailable: /Volumes/Scratch is not mounted
```

`--json` emits the same content for scripts.

## Verification

```sh
cargo test --locked --lib local_registry::tests -- --nocapture
cargo test --locked --test local_registry_cli -- --nocapture
cargo test --locked --test local_registry_portability -- --nocapture
cargo test --locked --test local_registry_vs_remote -- --nocapture
cargo clippy --locked --all-targets -- -D warnings
```

The CLI test points the registry at an empty `file://` directory, so any
install that succeeds there succeeded because the local registry satisfied it.
