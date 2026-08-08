# `zed graph github` production integration

This branch ports the certified GitHub repository/organization dependency inventory into `zed-cli` as a Rust-native modular command.

## Command

```console
zed graph github \
  --org zed-pkg-test \
  --repo zed-pkg-test/zed-pkg-e2e \
  --include zed,git-submodule,nix \
  --format json \
  --output inventory.json
```

The command emits `zpkg/github-dependency-inventory/v1` and reports `resolution=not-claimed`. It inventories exact source evidence; it is not the exact package resolver owned by DEN-2865.

## Conformance authority

- `zed-pkg-test/zed-pkg-e2e#133`: first deterministic repository/org inventory certification.
- DEN-2996 / `zed-pkg-test/zed-pkg-e2e#137`: flat `.zpkg.lock` selections are non-topological evidence.
- DEN-2997: workspace/member manifests and repository + manifest-path package identity.
- DEN-3003: corrected acquisition/parser hardening after a genuine exact-head branch and matrix exist.

The test-org fixtures remain authoritative until explicitly superseded by a versioned decision with byte-level cross-language evidence.

## Module ownership

| Area | Reserved production paths | Responsibility |
| --- | --- | --- |
| Core | `src/graph_github.rs` | CLI model, canonical inventory model, deterministic sorting, iterative SCC/cycles/waves, renderers, atomic output |
| Transport | `src/graph_github_transport.rs` | HTTPS policy, token routing, pagination, bounded retries/reads, redaction |
| Acquisition | `src/graph_github_acquire.rs` | org/repository discovery, exact commit/tree/blob acquisition, completeness |
| Parsers | `src/graph_github_parse*.rs` | Zed, lock-pin evidence, git-submodule/gitlink, Nix, workspace provenance |
| Dispatch | `src/main.rs` | minimal modular pre-Clap dispatch only |

No slice may edit DEN-2864 contract-authority paths or introduce exact resolution.

## Lock evidence rule

A flat lock closure can prove exact selections but cannot prove parent-child topology.

- valid lock entries are bounded, deduplicated pins with `topological=false`;
- pins never create nodes or edges by themselves;
- only a unique pin matching an already-proven direct declaration may annotate that edge;
- transitive/unmatched pins remain disconnected evidence;
- conflicting pins and declared direct dependencies missing from the lock are explicit contradictions;
- SCCs, cycles, and dependency-first waves operate only on proven edges;
- DOT and Mermaid render pins as disconnected evidence nodes.

## Required security properties

1. Tokens are read only from `ZED_PKG_GITHUB_TOKEN` or `GITHUB_TOKEN`, never argv.
2. Default token routing is restricted to `api.github.com` and loopback fake servers. A custom GitHub Enterprise origin requires explicit opt-in.
3. Pagination follows only same-origin `rel="next"` links and rejects cycles/cross-origin links.
4. Redirects are not followed with authorization.
5. Requests, retries, sleeps, response bytes, tree entries, manifests, fields, nodes, edges, pins, JSON depth, and wall time are bounded.
6. Response bodies from failed HTTP requests are never retained or surfaced.
7. Recursive-tree truncation, blob identity mismatch, invalid encoding, missing gitlinks, and malformed manifests are explicit failures.
8. Tokens cannot appear in errors, logs, output, graph labels, filenames, or artifacts.
9. Output is written by same-directory temporary file, synchronized, atomically replaced, and cleaned on failure.

## Required determinism properties

- normalize, deduplicate, and sort repeated repository/organization/include inputs;
- pin exact repository commit and manifest blob provenance;
- sort all semantically unordered collections and JSON keys;
- use iterative graph analysis and canonical SCC identifiers;
- produce dependency-first condensation waves;
- use stable UTF-8/LF encoding and format-specific DOT/Mermaid escaping;
- compare canonical JSON, DOT, and Mermaid bytes on Linux, macOS, and Windows;
- run a live `zed-pkg-test` organization smoke in addition to fake-server fixtures.

## Historical branch quarantine

The following branches are not integration authorities and must not be merged by name alone:

- `agent/zed-package-graph`
- `codex/zed-package-graph`
- `chore/zed-package-graph-20260805`

A primitive may be reused only after source-level review proves it respects the inventory/resolver boundary and passes relevant conformance tests.

## AI review

The durable provider-review queue is `ORESoftware/ai-agent-bridge.rs#104` / DEN-2871. A queued handoff is not an approval; provider/network/credit failures remain explicit.