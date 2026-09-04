# Static inspection protocol

`zed inspect --format json --root <project>` is the shared, versioned analysis
boundary for editor extensions and other read-only tooling. It emits exactly one
JSON document followed by a newline on stdout and does not write project state,
recover transactions, contact the network, or execute Git, mise, Nix, package
hooks, or package binaries.

Diagnostics do not make the command fail. Missing roots, invalid files, and
cross-tool incompatibilities are represented in the JSON response so clients
can always render a useful result. Process failure is reserved for failures to
serialize or write the protocol itself.

## Version 1.0 shape

The top-level response contains:

- `schema_version`: currently `"1.0"`;
- `producer`: the `zed-pkg` producer name and CLI version;
- `root`: an absolute project path;
- `package`: manifest, lockfile, and materialization paths;
- `interop`: independent Git-submodule, mise, and `nix develop` status;
- `summary`: health plus error, warning, and informational counts;
- `diagnostics`: deterministically sorted, stable diagnostic codes.

Each interop status distinguishes `detected`, `enabled`, and `ready`. This is
important for `.gitmodules`: mere presence is not permission for Zed to consume
it. A project opts in with:

```toml
[interop]
git-submodules = true
```

The analyzer reports an unclaimed `.gitmodules`, an opt-in with no file, an
invalid flag, unsafe file kinds, and Git lock extensions that no manifest owns.
Runtime commands still perform the stronger Git index and commit-provenance
checks before consumption.

## Environment compatibility

mise analysis parses the project-local `mise.toml` or `.mise.toml` and validates
`mise.lock` with the same frozen lock contract used by `zed env verify mise`.
It rejects ambiguous configs, unsupported inputs, missing lock coverage,
inexact versions, and artifacts without cryptographic checksums without running
mise.

Nix analysis follows `zed develop` precedence: `.nix/flake.nix` shadows a root
`flake.nix`. It checks adjacent lock placement and validates that `flake.lock`
is a UTF-8 JSON lock graph with a positive version and a resolvable root node.
It also flags `.envrc` requests for a missing flake. It never evaluates the
flake or invokes `nix develop`; evaluation remains a runtime boundary.

When both locked mise and Nix inputs are ready, the protocol reports their
layered composition: `nix develop` is the outer environment and mise is the
inner project-tool layer.

## Client rules

Editor clients should treat diagnostic `code` values and `schema_version` as
the compatibility surface. Messages and details are presentation text and may
improve over time. Unknown diagnostics must be displayed, not discarded.

Actions are declarative. Clients must never run a mutating action without an
explicit user gesture, and should enforce the supplied `mutates_project`,
`requires_network`, and `executes_package_code` risk flags. Version 1.0 currently
uses read-only open-file actions for static interoperability findings.

Clients may retain a local fallback analyzer for older CLIs, but a successful
version-1.0 response is authoritative. They should reject unsupported major
schema versions rather than guessing at their meaning.
