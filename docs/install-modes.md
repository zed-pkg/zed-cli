# Install modes and container ownership

Zed has two installation ownership models. Choose the model at the boundary
where the installed tree will be consumed; Zed never treats hardlinks as a
portable substitute for either one.

## Contract summary

| Mode | Project tree owns bytes? | Needs the Zed store at runtime? | Intended use |
| --- | --- | --- | --- |
| `symlink` | No | Yes | Local development on filesystems that support Zed's store-backed links |
| `copy` | Yes | No | Docker build contexts, OCI layers, deployment bundles, read-only runtimes, and other filesystem or mount boundaries |

`symlink` remains the default for local installations because one immutable
artifact in the content-addressed store can serve many projects. Removing the
store, omitting its mount, or moving the project independently breaks those
links by design. A project must not treat a symlink install as a deployable
bundle.

`copy` recursively materializes independent package files, adapter trees, and
hoisted executables below the project. The destination can be moved into a
Docker build context, copied between stages, exported as an OCI image, mounted
read-only, or run without the Zed store. Mutating a copied destination does not
mutate the content-addressed source or a second adapter copy.

On platforms where Zed cannot create its store-backed symlink layout, a
requested `symlink` installation is reported explicitly and executed as
`copy`. The final install summary reports the effective mode; there is no
silent mode substitution.

## Container recipe

Select copy mode explicitly at the dependency-install layer:

```dockerfile
FROM debian:bookworm-slim AS dependencies
WORKDIR /app
COPY .zpkg.toml .zpkg.lock ./
COPY --from=zed-cli /usr/local/bin/zed /usr/local/bin/zed
RUN --mount=type=cache,target=/root/.zed-pkg \
    zed install --frozen --install-mode copy

FROM gcr.io/distroless/base-debian12
WORKDIR /app
COPY --from=dependencies /app /app
USER 65532:65532
```

The cache mount accelerates downloads and extraction in the build stage. It is
not copied into the final image and is not required by the copied package tree.
The same rule applies when a package has a build step: build-cache output is
copied into the project before the OCI boundary.

Useful command forms are:

```sh
zed install --frozen --install-mode copy
ZED_PKG_INSTALL_MODE=copy zed install --frozen
```

Do not rely on environment inference in a release Dockerfile. Keeping
`--install-mode copy` next to `--frozen` makes the ownership decision visible
in code review and build logs.

## What copy mode preserves

A successful copy installation preserves:

- file contents and executable permissions;
- the configured `install.dir` tree;
- ecosystem adapter trees such as `node_modules/@org/name`;
- generated `.zed` wiring and `paths.json`;
- copied, executable entries in `<install.dir>/.bin`;
- deterministic `.zpkg.lock` restoration;
- selected polyglot target contents;
- declared build outputs after `--allow-build` or a prior `zed build`.

Workspace members follow the selected install mode too. `copy` snapshots the
member into the consumer; `symlink` keeps the live-edit local-development
behavior.

## Ownership and mutation

The global store and build cache are inputs. Zed extracts and builds there once,
then treats those trees as immutable. In copy mode every project destination is
an independently owned output. Neither project edits nor permission changes may
flow back to the store, build cache, workspace source, or a second adapter
materialization.

The permanent `copy-mode OCI contract` workflow verifies this with content
comparisons, device/inode checks where the platform exposes them, deliberate
destination mutations, removal of the store and build cache, a Docker build
context, OCI save/load, a non-root user, and a read-only runtime filesystem.

## Hardlinks

Hardlinks are not a public install mode or a correctness dependency. They do
not cross filesystems or mounts, still share an inode with the source, and are
therefore the wrong ownership model for a mutable project tree or an OCI
boundary.

A future hardlink experiment must remain opt-in and must:

1. prove source and destination are on the same supported filesystem;
2. report cross-device or unsupported cases rather than silently changing
   modes;
3. preserve permissions without mutating the immutable store inode;
4. materialize independent files before any Docker context, mount, archive, or
   OCI export boundary;
5. demonstrate a measurable benefit over copy mode.

Until those conditions are met, use `symlink` for store-backed local work and
`copy` for portable artifacts.
