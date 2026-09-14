# GitHub-only recovery when the Zed control plane is unavailable

`zed` must continue resolving and installing already-published public packages when every Zed-operated network service is unavailable. `registry.zpkg.net`, `cdn.zpkg.net`, Cloudflare Workers, R2, and application servers are acceleration and control-plane services; they are not required availability dependencies for public package restore when GitHub contains enough provenance to reconstruct the package.

## Recovery contract

The normal path remains the configured registry. When an HTTP registry request fails and source fallback is enabled, `zed` may reconstruct package metadata from GitHub and fetch package bytes from GitHub-owned surfaces. GitHub is treated as an independent content/repository recovery plane, not as a new package-authority model.

A public package is admissible through GitHub fallback only when the repository can be mapped unambiguously to the requested `org/name`, the committed `.zpkg.toml` self-claims that package identity, and the requested version is represented by an admissible version tag. Release assets, GHCR artifacts, and the GitHub tag archive are acceptable GitHub-hosted content sources. A tag archive is repacked using the same deterministic Zed package rules before its digest is recorded or compared.

The fallback remains fail-closed. Repository-search ambiguity, a mismatched package identity, malformed or unsafe archives, missing required provenance, and digest/size mismatches are errors. Fallback never grants authority to an arbitrary repository merely because its name resembles the package name.

## What must not be required

The recovery path must succeed with all of the following unavailable at the same time:

- the configured Zed HTTP registry;
- every `*.zpkg.net` endpoint;
- Cloudflare Workers, Pages, Tunnels, and other Zed-owned edge services;
- the Zed R2/CDN origin; and
- any Zed application or API server behind Cloudflare.

The exact-head outage workflow configures a guaranteed-unresolvable registry plus a dead loopback R2 endpoint. It then removes `GITHUB_TOKEN`, `GH_TOKEN`, and `ZED_PKG_GITHUB_TOKEN` and restores public packages from GitHub. This proves the public recovery path does not rely on credentials or on a Zed-operated server.

## GitHub as a recovery CDN

For public packages, the GitHub recovery plane can use:

1. `api.github.com` for repository identity, tags, releases, and tag-to-commit provenance;
2. `raw.githubusercontent.com` for the committed `.zpkg.toml` used for identity admission;
3. GitHub Releases and GitHub Packages/GHCR when a package publishes compatible artifacts there; and
4. GitHub/codeload tag archives as the final reconstructable source artifact.

When a GitHub source succeeds, Zed-owned mirrors are not required for correctness. R2 remains an optional mirror for cases where GitHub is unavailable or incomplete; it must not be the sole recovery source for a package that can be reconstructed from GitHub.

For private repositories, an explicit GitHub token may be used. Credentials are attached only to GitHub API requests that require them; redirects to codeload or unrelated mirror hosts must not receive the token.

## Hermetic exceptions

`file://` registries and loopback registries remain hermetic by default. Local tests, air-gapped mirrors, and deliberately isolated development environments must not leak package identities to GitHub. The explicit loopback fallback opt-in remains available for controlled canaries.

Source fallback is a read/recovery capability. Operations that require Zed control-plane state or mutation—publishing, namespace claims, yanking, server-side search, and audit-log access—may still fail while the control plane is down. The outage guarantee is specifically resolution, download, deterministic reconstruction, install, and frozen restore of packages whose GitHub provenance is sufficient.

## Acceptance gates

The required outage canary must prove all of these conditions on the exact pull-request head:

1. the configured registry is unreachable;
2. the configured R2/CDN endpoint is unreachable;
3. no GitHub token is available for the public-package test;
4. five package ecosystems resolve from GitHub source fallback;
5. the packages are actually materialized, not merely resolved;
6. a second empty store produces the identical lockfile;
7. a frozen restore succeeds from another empty store;
8. disabling source fallback makes the same outage fail instead of reusing an accidental cache;
9. a GitHub repository/tag with the wrong package identity is rejected; and
10. successful recovery has no dependency on a `zpkg.net` or Cloudflare response.

These gates intentionally model a total Zed control-plane outage rather than a partial CDN degradation.