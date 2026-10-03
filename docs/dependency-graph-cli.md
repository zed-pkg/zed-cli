# Dependency graph CLI

`zed graph` has two deliberately different graph operations:

- `zed graph package <org>/<name>@<version>` downloads the registry's immutable graph artifact for one exact package version.
- `zed graph local` resolves the complete **prospective** graph for a local `.zpkg.toml` without installing packages or writing a project lockfile.

The local command is intended for AI agents, editors, CI admission checks, dependency auditors, and other tools that need the whole selected graph before they decide whether to mutate a checkout.

## Fast local resolution

From a package root:

```sh
zed graph local
```

Or point at a manifest explicitly:

```sh
zed graph local --manifest path/to/.zpkg.toml
```

Compact deterministic JSON is written to stdout by default. Use `--pretty` for humans or `--output graph.json` for an atomic no-clobber file write.

The default graph includes runtime and build dependencies because both can affect a successful package operation. Use `--runtime-only` when a consumer specifically wants the runtime projection.

## Why this is faster than install/tree inspection

The ordinary installer must be able to materialize artifacts. Historical dependency inspection paths also rely on a lockfile and/or manifests from already-materialized dependencies. Neither is ideal for a fresh `.zpkg.toml`.

`zed graph local` instead asks the registry for each selected package version's immutable `view=declared` graph document. That small document carries the dependency requirements needed for recursive solving, so the client does not download and extract every package archive merely to discover its manifest.

The solver keeps package, candidate, and declared-graph metadata in memory for the duration of the calculation. Version selection is deterministic and backtracks only on semantic constraint conflicts. Transport, authentication, malformed graph, identity mismatch, and integrity errors are operational failures and are never reinterpreted as reasons to select an older version.

## Output contract

The prospective output uses `zpkg/local-dependency-graph/v1`, not the authoritative resolved `zpkg/dependency-graph/v1` shape. A pre-lock analysis does not yet possess the registry-snapshot and lock provenance required by the resolved wire contract, so it must not pretend to be that artifact.

The JSON contains:

- `root`: the local package coordinate;
- `nodes`: one exact selected version per package, including source (`root`, `registry`, `workspace`, or `path_override`) and registry artifact SHA-256 when applicable;
- `edges`: exact selected `from`/`to` coordinates plus the original requirement and dependency kind;
- `complete: true`: emitted only after the whole active graph resolves successfully;
- `stats`: registry reads, declared-graph cache hits, and any explicit artifact fallback work;
- `analysis_digest`: SHA-256 over the semantic graph fields (`schema`, `complete`, `root`, `nodes`, `edges`). Performance counters are intentionally excluded so repeated equivalent analyses have the same digest.

Nodes and edges are sorted and deduplicated before serialization. Compact output is therefore suitable for hashing, caching, diffing, and direct model/tool ingestion.

## Workspace and override behavior

The command honors workspace members and `[overrides.path]` using the same local manifest sources as package resolution. Explicit path overrides take precedence over workspace candidates.

The ambient machine-wide local registry is intentionally not consulted by this command. A graph intended for automation should not silently change because an unrelated checkout was registered on one developer machine. Put local dependencies in the workspace or declare a path override when they are part of the intended graph.

## Artifact fallback

Fast mode fails closed when an HTTP registry does not expose immutable declared-graph metadata. For an older registry or a `file://` registry, explicitly permit the traditional verified artifact-manifest path:

```sh
zed graph local --allow-artifact-fallback
```

That mode may download and extract package artifacts into the ordinary Zed store. The JSON `stats.artifact_downloads` and `stats.artifact_manifest_fallbacks` fields make that visible to automation.

## Safety and bounds

The metadata path:

- requires HTTPS outside explicit loopback registries;
- does not follow HTTP redirects;
- applies a 30-second request timeout;
- accepts at most 32 MiB per graph document by default (`--max-metadata-bytes` can lower the bound, but cannot exceed the shared graph-contract limit);
- requires canonical JSON with a valid semantic graph digest and verifies a present digest response header against the document;
- verifies requested package identity against both registry version metadata and declared graph identity;
- rejects cross-registry edges rather than silently resolving them against the wrong registry;
- caps recursive provenance depth at 256 and active package coordinates at 10,000;
- preserves the shared graph-contract limits of 50,000 nodes and 500,000 edges;
- bounds conflict provenance shown in diagnostics.

Output files are created atomically beside their destination and refuse to clobber an existing file.

## AI/tooling pattern

A tooling process can treat successful stdout as one self-contained dependency snapshot:

```sh
zed graph local --manifest .zpkg.toml > /tmp/graph.json
```

For a successful fast-path calculation, `stats.artifact_downloads` should be `0`. Cache downstream analysis by `analysis_digest`, then use the exact `nodes` and `edges` arrays for traversal, impact analysis, cycle detection, policy checks, or context selection.
