# Package environment contracts

SDK/library packages declare environment requirements in `.zpkg.toml`. This is separate from Zed's environment-plan/task manifests: the package declares **what it may read**, while the consuming process decides **how values are supplied**.

```toml
[env]
strict = false

[[env.vars]]
name = "ORES_LOCK_BACKEND"
type = "string"
default = "postgres"
enum = ["postgres", "redis", "durable_objects", "fiducia"]
description = "Distributed lock backend."

[[env.vars]]
name = "REDIS_URL"
type = "url"
secret = true
required_when = "ORES_LOCK_BACKEND == 'redis'"
```

Supported variable metadata in the first contract version: `name`, `type`, `required`, `required_when`, `default`, `secret`, `description`, `enum`, `deprecated`, and `replacement`. Types are `string`, `integer`, `boolean`, `url`, and `json`. Conditions intentionally start small: `VAR == 'value'` and `VAR != 'value'`.

Secrets must not have defaults. Package manifests describe secret *names and constraints*, never secret values.

`zed-env-contract check [path/to/.zpkg.toml]` validates the manifest and the current process environment. `list`, `json`, and `template` expose the same contract for humans, tooling, and CI. The template command redacts secret values and never materializes credentials.

## Ownership across the toolchain

- **zed-pkg / `.zpkg.toml`** owns package environment requirements, composition, and validation.
- **flags-2-env / `.cli-flags.toml`** owns CLI flag-to-environment normalization. A CLI may map flags to variables declared in its package env contract; zed-pkg must not duplicate flag parsing semantics.
- **ores-cli** owns fleet/repository linting. Its env-contract lint should compare statically discoverable environment reads against `.zpkg.toml`, and invoke the zed contract validator rather than creating another manifest grammar.

The next compatibility layer is transitive composition: downstream package graphs merge identically declared variables, retain source-package provenance, and reject incompatible declarations for the same variable name.
