# Local OCI image-layout materialization

`zed oci plan` can materialize the exact credential-free publication plan as a
standard OCI image-layout directory when `--out` is supplied:

```sh
zed oci plan \
  oci://ghcr.io/acme/tool:1.2.3 \
  --out dist/tool-layout

zed oci plan \
  oci://ghcr.io/acme/tool-rust:1.2.3 \
  --target rust \
  --out dist/tool-rust-layout \
  --json
```

Without `--out`, the command remains side-effect free and prints only the
immutable publication plan introduced by `zed-pkg/zed-cli#44`.

## Output

The output follows the OCI image-layout shape:

```text
dist/tool-layout/
├── oci-layout
├── index.json
└── blobs/
    └── sha256/
        ├── <config digest>
        ├── <package digest>
        ├── <source-manifest digest>
        └── <OCI manifest digest>
```

A dependency-bearing package also includes the exact `.zpkg.lock` bytes as a
typed blob. `index.json` points at the planned OCI manifest and preserves the
requested version tag through `org.opencontainers.image.ref.name`.

The resulting directory can be inspected, archived, copied into an OCI-aware
build pipeline, or used as the source for a later authenticated ORAS/registry
transport. This slice does not log in or publish anything.

## Integrity model

The publication planner remains authoritative. Materialization:

1. builds and validates the immutable plan;
2. repacks the selected repository or polyglot target in a temporary directory;
3. reconstructs the config, package, source manifest, optional lockfile, and OCI
   manifest bytes;
4. recomputes each SHA-256 and byte count;
5. rejects any difference from the plan;
6. writes each blob under `blobs/sha256/<digest>`;
7. writes `oci-layout` and `index.json` in a sibling temporary directory; and
8. renames the completed directory into the requested output path.

The command refuses to replace an existing output. A failed validation therefore
cannot partially overwrite a prior layout.

## Security boundary

Like plan-only mode, layout materialization returns before normal CLI
configuration, saved-credential resolution, or transaction recovery. It uses no
registry token, authentication session, socket, ORAS process, Docker daemon, or
Podman daemon.

The only durable write is the explicit `--out` directory. Temporary pack and
layout staging directories are removed on failure. The source project does not
receive `.zed/pack` output and pending `.zpkg-staging` state is not recovered or
mutated.

## Deliberately deferred

This PR does not implement:

- registry authentication or credential helpers;
- blob existence checks, upload, mount, or manifest push;
- tag replacement or mutable latest-tag policy;
- pull and digest-verified restore;
- OCI referrers for SPDX, CycloneDX, in-toto, or signatures;
- multi-platform image indexes across several target artifacts; or
- garbage collection of externally copied layouts.

Those remain separate contracts so a local byte-identity boundary can be tested
without network access or release credentials first.
