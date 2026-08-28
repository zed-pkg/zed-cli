# Package ignore rules

Zed packages can keep publish-only exclusions in either of two places:

- `.zpkg.toml` under `[publish].exclude`; or
- an optional artifact-root `.zedignore` file.

Use `.zedignore` when the manifest would otherwise become a long list of local,
hidden, generated, or editor-specific files. Keep rules in `.zpkg.toml` when they
are part of the package's visible release contract.

## Ordered union

When both sources contain rules, zed-pkg combines them in this order:

1. `[publish].exclude` rules from `.zpkg.toml`;
2. `.zedignore` rules.

The CLI emits one warning when both sources are active so the precedence is
visible. The union is not an error.

```toml
[publish]
exclude = [
  ".env*",
  "tmp/**",
]
```

```gitignore
# .zedignore
.idea/**
.cache/**
*.local.log
```

The example excludes all five rule families from the package artifact.

## Negation and contradictions

A leading `!` removes an earlier exclusion for the same normalized path family.
Because `.zedignore` is evaluated after the manifest, its rule wins when the two
sources disagree:

```toml
[publish]
exclude = ["target/**"]
```

```gitignore
# Keep a checked-in target directory.
!target
```

zed-pkg reports the conflict and keeps `target`. The reverse is also valid:
`!target` in the manifest followed by `target/**` in `.zedignore` excludes it.
Path-family comparison is case-insensitive, treats `\` as `/`, and normalizes a
leading `**/`, trailing slash, and trailing `/**`.

Repeated rules with the same polarity are harmless and are not reported as
contradictions. Blank lines and lines whose first non-whitespace character is
`#` are ignored.

## Hidden files

zed-pkg does not blanket-exclude every dotfile. Add only the patterns that match
the package's release policy, for example:

```gitignore
.env
.env.*
.direnv/**
.idea/**
.vscode/**
.cache/**
.DS_Store
```

For a single package or a whole-repository target, the artifact root is the
repository root. For a non-root polyglot target, it is that target's declared
`dir`; for example, `clients/ts/.zedignore` governs the re-rooted Node artifact.
A repository-root `.zedignore` does not leak into unrelated non-root targets.
When a target-local file and `[publish].exclude` are both active, diagnostics
name the target and its exact `.zedignore` path.

This avoids silently removing intentional package data. Built-in exclusions
still remove development machinery such as VCS metadata, CI configuration,
common test directories, dependency trees, and build-output directories.

`.zpkg.toml` and repository license/notice files are always retained even when a
user rule matches them. `.zedignore` is control metadata and is always excluded
from the published artifact; `!.zedignore` cannot re-include it.

## `.gitignore` is separate

`.gitignore` controls Git, not package contents. During `zed pack` and
`zed publish`, preflight rejects Git-ignored files that would otherwise remain
eligible for publication. Add those paths to `.zedignore` or
`[publish].exclude`, or explicitly re-include intentional generated package
inputs through the supported Zed inclusion mechanism.
