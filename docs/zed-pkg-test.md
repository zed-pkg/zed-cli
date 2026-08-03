# Testing `zed-cli` with `github.com/zed-pkg-test`

Every `zed-cli` pull request and `main` commit runs the reusable
`zed-pkg-test/zed-pkg-e2e` candidate smoke workflow from an exact harness
commit. The caller passes the exact CLI commit under test; the harness then
builds that commit once and runs the stateless package lifecycle against its
reviewed, commit-pinned fixture matrix.

The smoke workflow has read-only repository permissions, receives no secrets,
and fails closed when a root or transitive fixture dependency lacks an exact
commit. It is the fast pre-merge regression gate for release planning,
deterministic packing, `r2g`, dry-run and file-registry publication, discovery,
copy-mode installation, frozen replay, yank/undo behavior, package fan-out,
workspaces, vendoring boundaries, and non-package refusal.

This gate does not replace full candidate certification. Changes affecting
resolution, manifests or lockfiles, publishing, installation, registry or UI
semantics, authentication, Docker/OCI behavior, or cross-language fan-out must
also use a dedicated `zed-pkg-test/zed-pkg-e2e` pin PR and pass the lifecycle,
browser E2E, and install-boundary workflows against the same candidate SHA.
Record the smoke run and full-certification evidence on the owning Linear issue
under the `github.com/zed-pkg` project.
