# Authenticated OCI publication through ORAS

`zed oci push` copies one already-materialized Zed OCI image layout to a
registry through ORAS. It verifies the complete local layout before transport,
uses only an explicitly selected credential source, and resolves the remote tag
after the copy to prove that the registry now exposes the expected manifest
digest.

This command is intentionally separate from `zed publish`. The normal Zed
registry token, shared-auth session, project transaction recovery, and package
resolution path are not initialized.

## Prerequisites

Install an ORAS CLI that provides these capabilities:

- `oras cp --from-oci-layout`;
- `oras cp --to-registry-config`;
- `oras cp --no-tty`; and
- `oras resolve --registry-config`.

The contract tests use ORAS 1.3.2. The command checks the executable's advertised
capabilities before reading or mutating a remote tag. Override the executable
with `--oras /path/to/oras` or `ZED_PKG_OCI_ORAS`.

No Docker or Podman daemon is required. An explicit Docker-compatible registry
configuration may still be used for credential helpers.

## 1. Materialize the immutable layout

```sh
zed oci plan \
  oci://ghcr.io/acme/tool:1.2.3 \
  --out dist/tool-layout
```

For a polyglot package, select exactly one target during layout creation:

```sh
zed oci plan \
  oci://ghcr.io/acme/tool-rust:1.2.3 \
  --target rust \
  --out dist/tool-rust-layout
```

Treat the completed layout as immutable while a push is running. The command
rehashes every referenced blob immediately before invoking ORAS, but the ORAS
process subsequently opens the same layout path for the copy.

## 2. Select exactly one authentication mode

### Password or personal access token through stdin

```sh
printf '%s\n' "$REGISTRY_TOKEN" | \
  zed oci push \
    dist/tool-layout \
    oci://ghcr.io/acme/tool:1.2.3 \
    --username "$REGISTRY_USERNAME" \
    --password-stdin
```

The password or token is never placed in the ORAS argument vector. Zed creates a
temporary Docker-compatible registry configuration with mode `0600` on Unix,
passes that exact file to ORAS, and removes the temporary directory before the
command returns.

Do not put a token literal in shell history, a manifest, `.cli-flags.toml`, a
workflow file, or a repository URL.

### Explicit registry configuration or credential helper

```sh
zed oci push \
  dist/tool-layout \
  oci://ghcr.io/acme/tool:1.2.3 \
  --registry-config "$HOME/.docker/config.json"
```

This is an explicit opt-in to that file and any credential helper it names.
Zed never falls back to the default Docker configuration path when this option
is absent.

### Anonymous registry

```sh
zed oci push \
  dist/tool-layout \
  oci://registry.example/acme/tool:1.2.3 \
  --anonymous
```

Omitting every authentication mode fails closed.

Authentication and transport combinations are checked after flags-to-environment
normalization. This prevents generated `false` boolean defaults from being
mistaken for explicitly supplied conflicting flags while still rejecting
missing, partial, or conflicting modes before any credential file or network
transport is used.

## Local development registries

Unencrypted HTTP is accepted only for `localhost`, a `*.localhost` name, or an
IPv4 loopback address:

```sh
zed oci push \
  dist/tool-layout \
  oci://127.0.0.1:5000/acme/tool:1.2.3 \
  --anonymous \
  --plain-http
```

For a private TLS authority, use `--ca-file`. `--insecure-tls` is available for
explicit exceptional use and cannot be combined with `--ca-file` or
`--plain-http`.

## Verification before transport

Before ORAS runs, Zed requires:

1. `oci-layout` version `1.0.0`;
2. an OCI index with exactly one image-manifest descriptor;
3. `org.opencontainers.image.ref.name` equal to the destination tag;
4. one Zed config descriptor;
5. exactly one package archive or binary layer;
6. exactly one source-manifest layer;
7. at most one lockfile layer;
8. canonical lowercase SHA-256 descriptor digests;
9. descriptor sizes equal to actual file sizes;
10. every blob hash equal to its filename and descriptor; and
11. no missing, duplicate, symlinked, or undeclared blob entry.

A caller-selected digest is not accepted in the destination. The verified local
manifest determines the immutable digest.

## Remote-tag behavior

Zed resolves the destination tag before copying:

| Remote state | Result |
| --- | --- |
| tag is missing | copy, verify, return `pushed` |
| tag already has the same digest | do not copy, return `already-present` |
| tag has a different digest | fail closed |
| different digest plus `--allow-tag-replacement` | copy, verify, return `replaced` |

After every copy, Zed resolves the tag again and requires the exact verified
local manifest digest.

```sh
zed oci push \
  dist/tool-layout \
  oci://ghcr.io/acme/tool:1.2.3 \
  --registry-config "$DOCKER_CONFIG/config.json" \
  --allow-tag-replacement \
  --json
```

### Concurrency limitation

The pre-copy resolve and the registry copy are not an atomic compare-and-swap.
Another publisher can change the same tag between those operations. Use unique,
immutable version tags and enforce tag immutability in the registry. The
post-copy resolve detects a final digest mismatch, but it cannot make an
optimistic tag replacement atomic. A future native registry transport may add a
registry-specific conditional-write contract where supported.

## Interactive confirmation

`--interactive` adds a terminal confirmation immediately before a missing or
different remote tag is mutated. Redirected stdin never counts as interactive
consent. An already-present digest requires no confirmation.

## Machine-readable result

`--json` emits `zed.oci-push-result/v1`, including:

- the digest-qualified destination;
- verified manifest descriptor;
- `pushed`, `replaced`, or `already-present` status;
- selected authentication mode;
- ORAS version string;
- blob count and total bytes; and
- explicit transport-security exceptions.

Passwords, tokens, and registry configuration contents are never included.

## Deliberately deferred

This boundary does not yet implement:

- digest-verified pull and restore;
- recursive referrer copying;
- SPDX or CycloneDX attachment;
- in-toto or SLSA provenance attachment;
- signature publication and verification;
- cloud-registry identity federation;
- client-certificate or identity-token authentication;
- atomic conditional tag replacement; or
- a multi-platform index assembled from several Zed target layouts.