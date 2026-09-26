# Private GitHub package sources

Zed can resolve a package from a private GitHub repository through the existing source-fallback path without putting credentials in `.zpkg.toml`, `.zpkg.lock`, command-line arguments, generated configuration, artifact URLs, caches, or exported fetch metadata.

## Credential boundary

For GitHub source fallback, the CLI reads the first non-empty value in this order:

1. `ZED_PKG_GITHUB_TOKEN`
2. `GITHUB_TOKEN`
3. `GH_TOKEN`

CI should map its approved read-only secret into `ZED_PKG_GITHUB_TOKEN`. Do not write the token into a repository URL or package manifest. A token used to read a private source repository needs only the GitHub permissions required to read that repository and its source/release metadata.

Example workflow boundary:

```yaml
env:
  ZED_PKG_GITHUB_TOKEN: ${{ secrets.FLEET_GITHUB_READ_TOKEN }}
```

If the dependency graph contains private authorities, the workflow should fail closed when that secret is unavailable rather than silently deleting the dependency, weakening provenance, or treating a private 404 as proof that the package does not exist.

## Transport rules

Private tag archives use GitHub's REST tarball endpoint. The bearer credential is attached only to `api.github.com`; GitHub's redirect to the signed codeload URL must not carry that Authorization header. Repository/tag/manifest admission remains bound to the package's declared GitHub identity and committed `.zpkg.toml`.

Frozen registry sources reject embedded userinfo, passwords, queries, and fragments. Credentials therefore do not participate in lock identity, artifact digests, or reproducibility.

## Cross-organization CI

GitHub's normal `GITHUB_TOKEN` is repository-scoped and is not a general cross-organization private-source credential. A workflow in another organization must be explicitly provisioned with an approved read token that can access the private authority, then map that secret to `ZED_PKG_GITHUB_TOKEN`.

The `tests/private_github_auth_contract.rs` regression suite verifies these invariants without requiring a live secret. Live canaries remain separate because absence of an authorized secret is an operational authorization failure, not permission to weaken the package graph.
