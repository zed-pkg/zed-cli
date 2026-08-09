# Private R2 release-artifact mirror

The GitHub `release` workflow remains the build authority. The R2 publication
workflow mirrors one completed release run; it does not rebuild binaries, create
a tag, publish a GitHub Release, modify DNS, configure a Worker, or make the R2
bucket public.

## Secret boundary

Configure these as secrets on the protected GitHub environment named
`r2-release-publication`:

- `CLOUDFLARE_ACCOUNT_ID`
- `R2_ACCESS_KEY_ID`
- `R2_SECRET_ACCESS_KEY`

Do not put their values in a workflow, repository variable, issue, pull request,
release note, publication report, command-line argument, or checked-in `.env`
file. The Cloudflare API token is not required by this S3-compatible data path.

The workflow has only `actions: read` and `contents: read`. It downloads the
seven target archives from the selected GitHub Actions run, verifies all seven
per-target checksum files, creates an aggregate `SHA256SUMS`, and then enters the
R2 environment boundary.

## Object layout

For release `v0.1.0-rc.3`, the default private layout is:

```text
s3://zed-pkg-releases/zed-cli/v0.1.0-rc.3/
├── SHA256SUMS
├── SOURCE.json
├── zed-aarch64-apple-darwin.tar.gz
├── zed-aarch64-apple-darwin.tar.gz.sha256
├── zed-aarch64-unknown-linux-gnu.tar.gz
├── zed-aarch64-unknown-linux-gnu.tar.gz.sha256
├── zed-aarch64-unknown-linux-musl.tar.gz
├── zed-aarch64-unknown-linux-musl.tar.gz.sha256
├── zed-x86_64-apple-darwin.tar.gz
├── zed-x86_64-apple-darwin.tar.gz.sha256
├── zed-x86_64-pc-windows-msvc.zip
├── zed-x86_64-pc-windows-msvc.zip.sha256
├── zed-x86_64-unknown-linux-gnu.tar.gz
├── zed-x86_64-unknown-linux-gnu.tar.gz.sha256
├── zed-x86_64-unknown-linux-musl.tar.gz
└── zed-x86_64-unknown-linux-musl.tar.gz.sha256
```

Each object carries private immutable-cache metadata plus its SHA-256, release,
source repository, and source workflow-run ID. Publication performs a HEAD after
every PUT and fails unless the stored length and SHA-256 metadata match.

## Immutability and retries

The default behavior is fail-closed:

- an absent bucket is an error unless `create_bucket` is explicitly enabled;
- an identical object is reported as `unchanged`, making retries idempotent;
- a non-identical existing object is rejected unless `overwrite` is explicitly
  enabled; and
- symlinks and special files are rejected before publication.

Normal release-candidate publication should leave `overwrite` disabled. A new
build belongs under a new release namespace rather than replacing bytes already
reviewed under an existing name.

## Publishing the merged release candidate

Run **publish release artifacts to private R2** with:

```text
source_run_id: 31035722733
release:        v0.1.0-rc.3
bucket:         zed-pkg-releases
prefix_root:    zed-cli
create_bucket:  true only on the first publication
overwrite:      false
```

Run `31035722733` contains the seven artifacts built from merged commit
`2f98df7b0f3f20bd8eaec6abbe768566833589bc`. After the first successful
publication, rerun with `create_bucket: false`; every object should report
`unchanged`.

The workflow retains `SHA256SUMS`, `SOURCE.json`, the credential-free plan, and
the verified publication report as a 30-day GitHub artifact. Those files are the
review evidence; credentials are never included.
