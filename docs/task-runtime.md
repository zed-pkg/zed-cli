# Native task runtime

`zed task` is the canonical execution surface for schema-v2
`EnvironmentPlanV2` tasks. It is intentionally manager-neutral: mise, asdf,
Devbox, Flox, Nix, or native Zed adapters must first produce the shared plan
contract. The runtime does not parse manager configuration or execute manager
plugins.

The separately installed `zed-task` binary remains a compatibility and focused
certification surface. Both binaries call the same `zed_cli::task_cli` command
layer, so parsing after the command boundary, output, execution, error, cache,
and observer semantics do not diverge.
`zed-task` is the staged execution surface for schema-v2 `EnvironmentPlanV2`
tasks. It is intentionally manager-neutral: mise, asdf, Devbox, Flox, Nix, or
native Zed adapters must first produce the shared plan contract. The runtime
does not parse manager configuration or execute manager plugins.

## Commands

```console
zed task [--plan zed-env.toml] [--json] list [--all]
zed task [--plan zed-env.toml] [--json] info <task>
zed task [--plan zed-env.toml] [--json] graph <task>
zed task [--plan zed-env.toml] run <task> [--dry-run] [--yes] [--jobs N] [--no-cache] [-- <args>...]
```

Compatibility spellings use the same trailing arguments after replacing
`zed task` with `zed-task`.

`--jobs` is a positive integer. Zero is rejected during argument parsing before
plan discovery, so a malformed concurrency request cannot partially inspect or
execute a project plan. The same validation applies to `ZED_TASK_JOBS` after
flags-to-environment normalization.

Every option has one environment identity in the main `.cli-flags.toml` and the
staging `.task-cli-flags.toml` contract:
zed-task [--plan zed-env.toml] list [--all]
zed-task [--plan zed-env.toml] info <task>
zed-task [--plan zed-env.toml] graph <task>
zed-task [--plan zed-env.toml] run <task> [--dry-run] [--yes] [--jobs N] [--no-cache] [-- <args>...]
```

`--jobs` is a positive integer. Zero is rejected during argument parsing before
plan discovery, so a malformed concurrency request cannot partially inspect or
execute a project plan. The same validation applies to `ZED_TASK_JOBS` after
flags-2-env normalization.

Every option has one flags-2-env identity in `.task-cli-flags.toml`:

- `ZED_TASK_PLAN`
- `ZED_TASK_JSON`
- `ZED_TASK_ALL`
- `ZED_TASK_DRY_RUN`
- `ZED_TASK_YES`
- `ZED_TASK_JOBS`
- `ZED_TASK_NO_CACHE`

The canonical route reuses these names rather than creating a second
configuration plane.

## Supported execution semantics

The native slice supports:
The eventual `zed task` integration must reuse those names rather than creating
a second configuration plane.

## Supported execution semantics

The first native slice supports:

- validated task and alias lookup;
- `depends`, `wait_for`, and `depends_post` ordering;
- ordered shell command lists;
- nested task invocations with task-local scalar environment and arguments;
- sequential or bounded parallel task groups;
- platform-specific `run_windows` selection;
- project-local working directories with canonical symlink containment;
- scalar top-level/task/invocation environment precedence;
- explicit confirmation through `--yes`;
- non-login shell execution and exact child failure propagation;
- `ms`, `s`, `m`, and `h` command timeouts;
- deterministic list, info, graph, dry-run, and JSON output; and
- content-verified incremental caching for declared sources and outputs.

Arguments are never interpolated into a shell command. They are exposed as:

```text
ZED_TASK_ARGC
ZED_TASK_ARGS_JSON
ZED_TASK_ARG_0
ZED_TASK_ARG_1
...
```

This avoids inventing an unsafe quoting policy before a typed usage/argument
contract is certified.

## Incremental cache

Caching is enabled only when a task explicitly sets `cache = true` and declares
both `sources` and `outputs`.

The cache identity includes:

- canonical environment-plan JSON;
- task definition and invocation identity;
- argument and invocation-environment identity;
- inherited process-environment identity;
- exact source paths and bytes; and
- the selected platform/runtime plan.

A hit additionally requires every declared output to exist with the same path
set and content digest. Cache records contain only task names and SHA-256
identities; no environment value or secret is serialized. Symlinks, path
escape, malformed cache state, missing outputs, excessive entry counts, and
excessive byte counts fail closed.

## Deliberate fail-closed boundaries

The runtime does not yet execute:
The staged runtime does not yet execute:

- task-local tool overlays;
- manager templates or task `vars`;
- arbitrary manager extensions;
- secret-provider references;
- task usage-schema parsing;
- process/network/filesystem sandboxes; or
- JSON reports mixed with live child stdout.

A plan containing one of those execution-time semantics is rejected instead of
silently losing it. Structured environment arrays/tables are likewise rejected
when converting to operating-system process variables.

The next certified slices add native tool activation, typed usage arguments,
capability profiles/process isolation, cancellation of complete descendant
process trees, and watch mode.
process trees, watch mode, and canonical `zed task` routing.
