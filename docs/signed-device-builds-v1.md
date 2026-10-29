# Signed device-build statements — experimental, not release-trusted

The new zed build-attest command signs an existing compiled artifact's
actual SHA-256 and size with an existing Zed publisher key. It does not compile
a source file, fetch credentials, issue or consume a server challenge, upload
an artifact, or certify that a builder ran.

Example (the placeholder values must come from a REAL authorized Oreslang API
challenge once its production endpoints are implemented):

    zed build-attest \
      --org acme --package widget --version 1.2.3 \
      --device-key-id device-1 \
      --challenge-id 12345678-1234-1234-1234-123456789abc \
      --nonce-sha256 [64-lowercase-hex-of-server-nonce] \
      --source-commit [40-lowercase-hex] \
      --source-sha256 [64-lowercase-hex] \
      --toolchain-sha256 [64-lowercase-hex] \
      --target llvm --artifact-format tar.gz \
      --issued-at-unix-ms [challenge-issue-time] \
      --expires-at-unix-ms [challenge-expiry] \
      --artifact dist/widget.tar.gz \
      --output dist/widget.signed-build.json

The signing key is loaded through the existing Zed publisher-key store
(zed key generate / zed key enroll), not through an upload token. The
signature is over the domain-separated versioned Zed build claim; it cannot
be misinterpreted as an ordinary version or mirror metadata signature.

The signer refuses missing and empty artifact files, symlink paths, invalid
claims, and existing sidecar destinations; it never overwrites an existing
receipt. It reads actual artifact bytes in bounded chunks and publishes a
sidecar via a fresh-file create. The source/tree/toolchain values remain
client claims; they require independently verified provenance later.

Separate registry verification endpoint (draft):
    POST /v1/orgs/{org}/signed-builds/verify

The registry authorizes the caller, verifies a currently ACTIVE key and
compares signed claims to actual stored package/version bytes. It returns
signed-self-report-not-compiled-proof even if valid. It neither verifies
a real compiler execution nor atomically consumes the Oreslang build
challenge. A future verified-release policy must require an authenticated
Oreslang API receipt or a trusted independent rebuild.

This is an additive opt-in workflow. zed build remains the dependency
build command and zed publish retains existing publisher-metadata signing;
this draft does not silently enable a signed-build release requirement.
Use strict fail-closed enforcement only after challenge issuance, upload,
trust-tier persistence, and independent build verification are complete.
