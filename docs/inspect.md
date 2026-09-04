# Local inspection contract

`zed inspect` is the common, read-only diagnostics boundary for editor
extensions, CI checks, and agent tooling:

```sh
zed inspect --format json --root /absolute/project/root
```

The command writes exactly one UTF-8 JSON document to stdout. It does not load
credentials, contact the registry, download packages, execute package code,
repair transactions, synchronize Git submodules, or write project state.
Malformed manifests and `.gitmodules` files become structured diagnostics
instead of unstructured parser failures.

The response uses `schema_version = "1.0"`; its checked-in JSON Schema is
[`schemas/inspect-v1.json`](../schemas/inspect-v1.json). Additive fields may be
introduced within major version 1. Consumers must reject an unsupported major
version and ignore unknown additive fields from a supported major version.

## Declaring `.gitmodules` ownership

A repository that wants Zed to consume its committed `.gitmodules` file
declares that policy in `.zpkg.toml`:

```toml
[interop]
git-submodules = true
```

This becomes the default for `zed install` and `zed overtake`. Precedence is:

1. an explicit `--git-submodules=true` or `--git-submodules=false`;
2. `ZED_PKG_GIT_SUBMODULES`, when set;
3. `[interop].git-submodules`;
4. off.

Use `git-submodules = false` to document that Git alone owns `.gitmodules`.
Leaving the key absent while `.gitmodules` exists produces the stable
`GITMODULES_UNDECLARED` warning, because an editor or CI runner cannot otherwise
know whether Zed is expected to validate and synchronize that graph.

## Cross-compatibility checks

The v1 report statically checks:

- manifest intent versus `.gitmodules` presence;
- regular-file and stage-zero Git index safety for both metadata files;
- complete, unique, safe submodule paths and credential-free URLs (including
  rejection of Git's command-executing `ext` transport);
- committed `.gitmodules` provenance;
- initialized, exact, clean submodule checkouts, including nested submodules;
- overlap between `[install].dir` and Git-owned paths;
- duplicate upstream repositories and duplicate Zed package identities;
- registry dependencies that duplicate an unadopted submodule;
- exact adopted dependency versions and workspace membership; and
- package-repository URL drift from the committed submodule transport.

Every submodule entry reports its path, transport, initialization state,
verified commit when available, detected Zed package identity, and workspace
adoption state. Diagnostics use stable symbolic codes and absolute paths.
Suggested commands are argv arrays, never shell strings, and explicitly label
project mutation, network access, and package-code execution.

## Editor policy

VS Code, Eclipse, Xcode, Sublime Text, IntelliJ, and other integrations should
invoke this command in a background process and render the returned diagnostics
without reimplementing Git or TOML heuristics. Editors must still enforce their
own safety boundary:

- never run a suggested action during background refresh;
- accept only known action kinds and a supported schema major version;
- require explicit confirmation for every project mutation;
- execute command arrays without a shell;
- require the first argv element to resolve to the configured zed-pkg CLI; and
- treat `executes_package_code = true` as a stronger consent boundary.

An integration may retain a local fallback for machines with an older CLI, but
the fallback should surface an explicit “inspection protocol unavailable” state
instead of silently claiming full cross-compatibility coverage.
