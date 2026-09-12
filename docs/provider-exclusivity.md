# Exclusive providers

A package can declare an exclusive implementation in `zed-provider.toml` at
the root of its published artifact:

```toml
schema = 1
group = "ores-dnd/browser"
provider = "pragmatic"
```

The group is a namespace and capability separated by `/`. Both parts and the
provider name are lowercase alphanumeric slugs with internal hyphens, at most
64 bytes each. Unknown fields, unsupported schemas, oversized declarations,
non-files and symbolic links fail admission. Diagnostics never quote the input.

Declare this file inside each target directory, beside its independent native
manifest. Use a distinct `[targets.<name>]` and published package name for each
implementation. Omit a whole-repository target when it would ship competing
implementations together. Keep backend dependencies inside their native target
manifest; root dependencies are inherited by every target.

Before native package managers, dependency build/install hooks, lockfile writes
and consumer wiring, `zed install` checks every resolved package, the root
selection and workspace links. This applies to fresh and frozen installs and
includes transitive packages. Two different providers in the same group stop
the install. Multiple packages from the same provider are allowed, so a browser
engine and its WASM bridge can coexist. Different groups are independent.

The declaration travels in the checksum-verified artifact. It supplements
existing target pruning; it does not change manifest or lockfile schemas or
require a registry metadata migration. Packages without declarations preserve
their existing behavior. This is a declared dependency invariant, not detection
of undeclared third-party engines in arbitrary source code or a security sandbox.

This capability first appears in the provider-exclusivity change; released
Zed 0.3.0 predates it. Consumers requiring enforcement must use a build containing
that change until a release includes it. Do not claim enforcement from target
names alone. External ORES consumer tests live in
`ores-dnd-test/packaging-isolation`.
