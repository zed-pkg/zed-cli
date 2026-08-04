# Current `mise.lock` identity contract

`zed-cli::mise_lock` models and validates the complete current project-local
`mise.lock` identity without invoking mise, fetching artifacts, executing hooks,
or reading user-global configuration.

This contract is intentionally separate from the smaller compatibility subset
used by `zed env import mise` today. The importer remains backward compatible
while this module establishes the lossless foundation for:

- complete frozen verification;
- deterministic import/export round trips;
- translation into Zed's native `EnvironmentLock`;
- independent tamper and offline-replay certification; and
- migration diagnostics for state that cannot yet be represented by the
  common `EnvironmentPlan` importer.

## Represented state

The contract preserves:

- every `[[tools.<name>]]` version/backend/options identity;
- all locked platform variants;
- compact checksum strings and detailed platform tables;
- `install = "source"` authoring state;
- checksum, size, download URL, API URL, and ordered additional artifacts;
- minisign, cosign, SLSA, and GitHub-attestation provenance state;
- verification status;
- conda and pkgx dependency references;
- shared `[conda-packages.<platform>]` and `[pkgx-packages.<platform>]`
  sections;
- pkgx-provided executables and runtime environment; and
- option-dependent and multi-version identities.

Unknown fields fail closed. They are not discarded and therefore cannot be
hidden by a successful import or digest.

## Current wire format

Current mise serializes platform identities as quoted literal keys beneath each tool identity:

```toml
[[tools.actionlint]]
version = "1.7.12"
backend = "aqua:rhysd/actionlint"

[tools.actionlint."platforms.linux-x64"]
checksum = "sha256:..."
url = "https://..."
url_api = "https://api.github.com/..."
provenance = "github-attestations"
```

The parser accepts this current wire form and the earlier nested compatibility form, but rejects a single identity that mixes both encodings. Deterministic TOML output always uses the quoted current `"platforms.<target>"` form. A fixture copied from mise commit `72379d0c459808f980a037065ac9c39a60032280` proves parse, deterministic render, reparse, and semantic-digest equality without invoking mise.

## Validation modes

### `Authoring`

Validates the structure and relationships of a manager lock while allowing
source-build-only identities and optional checksums. This is suitable for
inspection and deterministic round-trip work, but does not claim that another
machine can replay every artifact.

### `FrozenPortable`

Adds portable restore requirements:

- every tool has at least one platform identity;
- every platform and additional artifact has a supported cryptographic
  checksum;
- shared conda/pkgx packages are checksummed;
- `install = "source"` is rejected;
- moving versions are rejected;
- all shared dependency references resolve on the exact platform; and
- network locations are credential-free and contain no fragments or
  secret-bearing query parameters.

Validation is read-only and performs no network access.

## Determinism

`normalized()` canonicalizes non-semantic state:

- tool identity arrays retain declared order because multi-version activation is order-sensitive;
- compact platform checksums become detailed platform records;
- checksum algorithms and digests become lowercase;
- set-like dependency/provides lists are sorted and deduplicated; and
- maps retain deterministic `BTreeMap` ordering.

Ordered tool identities and `additional_artifacts` remain ordered because mise
uses their sequence during activation and extraction. Reordering either changes the
semantic digest.

`canonical_json_bytes()` emits compact canonical JSON. The lock identity is a
domain-separated SHA-256:

```text
SHA256("zed-pkg:mise-lock-identity:v1\0" || canonical-json)
```

`to_toml_string()` emits deterministic normalized TOML. The generated mise
comment header is presentation metadata outside the TOML document and is not
part of the semantic digest.

## Security boundary

The contract rejects:

- embedded URL usernames or passwords;
- malformed textual schemes, missing URL hosts, and URL fragments;
- common token, credential, signature, API-key, and authorization query keys;
- malformed or unsupported checksum algorithms;
- zero-byte sizes when a size is present;
- `provenance_verified = true` without provenance;
- contradictory verified provenance and unavailable GitHub-attestation state;
- duplicate option-dependent identities;
- duplicate additional-artifact URLs;
- missing shared conda/pkgx package references;
- invalid runtime environment keys; and
- control-bearing or whitespace-padded identities.

The model does not store credentials and must not be extended to serialize
secrets. Signed URLs should be treated as ephemeral transport data rather than
portable committed lock identity.

## Deliberate next steps

This module does not yet change `zed env import mise` output. Follow-up work
will:

1. bind this exact lock digest to the imported environment plan;
2. compare every configured requirement/backend/platform against all matching
   lock identities;
3. translate portable identities into native `EnvironmentLock` variants;
4. add `zed env export mise --check|--write` with three-way conflict handling;
5. certify import → export → import identity across Linux, macOS, and Windows.

Tracked by Linear DEN-1461, DEN-1481, and DEN-1462.
