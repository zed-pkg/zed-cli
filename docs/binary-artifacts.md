# Native binary ZIP publication

`zed-binary` publishes a native executable as a deterministic ZIP. The source
release remains `org/name@version`; a binary artifact is additionally qualified
by its normalized target and archive format. A target is never appended to, or
encoded in, the package version.

## Archive contract

Every archive has one `pkg/` root:

```text
pkg/
  .zpkg.toml
  .zpkg-binary.json
  bin/
    tool
  lib/                 # optional, explicitly included runtime payload
  LICENSE              # optional legal payload
```

The ordinary `.zpkg.toml` is a sibling of `bin/` and `lib/`. Its `[bin]` table
remains authoritative. The generated `.zpkg-binary.json` binds package identity,
the complete structured platform, entrypoints, source provenance, and every
payload file's size, SHA-256, and executable intent. The descriptor excludes
itself to avoid a circular digest; the SHA-256 of the complete ZIP is the outer
blob identity.

The canonical writer emits only files, in lexicographic UTF-8 path order. It
uses the DOS epoch (`1980-01-01T00:00:00`), Unix mode `0644` or `0755`, and
DEFLATE level 6. It emits no comments or ambient filesystem metadata. Payloads
are opened and hashed during collection, then reopened and rebound to the same
filesystem object before ZIP creation without retaining one descriptor per
payload. A later path replacement or in-place mutation fails identity or digest
verification instead of producing a torn archive.

The verifier accepts Stored or Deflated entries for interoperability but rejects
encrypted entries, self-extracting prefixes, overlapping ranges, symlinks,
non-regular types, traversal, noncanonical names, portable case collisions,
Windows device/reserved names, trailing dot/space components, hidden directory
data, local/central header disagreement, data descriptors, unnecessary ZIP64,
unlisted or missing files, mode drift, digest drift, excessive compression, and
configured byte/entry limits. It hashes the same open archive handle before and
after all inspection so path swaps and concurrent mutation cannot produce a
stale successful result.

## Registry identity and compatibility

The default route remains the deployed one-artifact compatibility API:

```text
PUT /v1/packages/{org}/{name}/versions/{version}
GET /v1/packages/{org}/{name}/versions/{version}
```

Use `--artifact-route qualified` only with a registry implementing the additive
multi-artifact API:

```text
PUT /v1/packages/{org}/{name}/versions/{version}/artifacts/{target}/{format}
GET /v1/packages/{org}/{name}/versions/{version}/artifacts/{target}/{format}
```

The qualified PUT sends `zpkg.binary-artifact-publish-meta/v1`, including the
canonical descriptor SHA-256, and both PUT and GET receive
`zpkg.binary-artifact-metadata/v1`. That response retains the complete platform,
descriptor digest, nested source provenance, download URL, and optional evidence;
it is not the legacy version-metadata response shape.

For example:

```sh
zed-binary publish \
  --target aarch64-linux-android --os android --arch aarch64 --abi api24 \
  --artifact-route qualified

zed-binary download acme/tool@1.2.3 \
  --target aarch64-linux-android --artifact-route qualified \
  --out tool-android.zip
```

Qualified downloads require an exact target. Legacy publication fails if the
version already contains different bytes and points to the qualified route;
it does not invent a SemVer variant. Publishing the same identity and bytes is
idempotent. If an upload response is lost after the registry commits, the CLI
re-reads the same identity and recovers only when format, size, and digest all
match.

Both pack and download stage within the destination directory and fully verify
before promotion. Promotion creates a no-clobber hard link, which makes the
complete file visible atomically. An identical destination is accepted as an
idempotent retry; a different file, directory, or symlink is never removed or
overwritten.

## R2 and Android handoff

The R2 certification publishes through the registry, verifies the exact object
metadata, downloads through the registry, direct S3-compatible API, and a
five-minute presigned HTTPS URL, and requires all three ZIPs to have the same
SHA-256. The workflow mints a one-hour credential scoped to the single
content-addressed object. Secret-bearing `curl` calls permit HTTPS redirects
only, temporary credentials are masked and cleared, and the ephemeral object is
deleted and checked for absence.

The retained CI evidence certifies an Android ARM64 ELF and its
`/system/bin/linker64` interpreter. It deliberately reports
`physical_device_executed: false`: moving bytes through R2 is not evidence that
a connected phone installed or executed them. A device handoff must first use
`zed-binary verify --target aarch64-linux-android`, extract only the declared
entrypoint into a private staging directory, compare its descriptor digest, and
then use an explicitly selected `adb` device. Presigned URLs and R2 credentials
must never be placed in `adb` arguments, logs, or the device filesystem.
