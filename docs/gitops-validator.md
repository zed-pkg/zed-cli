# `zed gitops` external validator

`zed gitops` is the read-only GitOps validation lane tracked by DEN-2725. The
root `zed` binary now securely dispatches this command to the separately built
`zed-gitops` executable, while root help and shell completions expose the same
public command contract.

Install or build both binaries into the same bin directory:

```console
cargo install --path . --bins
zed gitops validate --root . --offline --strict
zed gitops validate --root . --offline --changed-from origin/main --format json --strict
zed gitops validate --root . --offline --strict --format sarif
```

The standalone spelling remains supported for automation that deliberately
pins the validator executable:

```console
zed-gitops validate --root . --offline --strict
```

The dispatcher resolves `zed-gitops` beside the running `zed` executable first,
then searches only absolute `PATH` entries. It never invokes a shell, never
searches the current working directory implicitly, never permits an extension
to shadow a built-in command or alias, and preserves the child process exit
code. Root options placed before or after `gitops` are passed as their canonical
`ZED_PKG_*` environment variables rather than being exposed on the child
command line. A literal `--` ends global-option extraction and passes every
remaining argument to the external command unchanged.

## Evidence checked

Generic gitlink contract (loaded from the target repository, not hardcoded
Kubernetes/Argo policy):

- `catalog/gitops/gitlink-contract.v1alpha1.json` when present, or `--schema`;
- `.gitmodules` and indexed mode-160000 gitlinks are parsed as local evidence;
- application gitlinks must live under `spec.approvedAppPathPrefixes`;
- gitlinks under those prefixes that are missing from `spec.allowedGitlinks`
  fail as unexpected;
- untracked directories that contain a `.git` marker under an approved prefix
  fail as submodule impersonators;
- `--changed-from` compares the current gitlink set to an already-fetched local
  ref such as `origin/main` and records the changed paths.

Catalog/Argo checks remain repository-owned JSON under `--catalog` and are
skipped when that directory is absent:

- catalog JSON is regular UTF-8 data beneath the selected repository root;
- unknown fields fail under `--strict`;
- `.gitmodules` provides exactly the declared inventory path and repository;
- the Git index contains that path as a mode-160000 gitlink;
- catalog inventory revision equals the indexed gitlink SHA;
- Argo source repository canonicalizes to the same upstream repository;
- Argo `targetRevision` is an exact lowercase 40-hex commit equal to the
  gitlink;
- the source is the direct app repository, not a path inside
  `ORESoftware/k8s-cluster`;
- application names and inventory paths are unique;
- `*-infra` repositories cannot be app records;
- AppProject and destination namespace cannot be `default`;
- `pilot-inert` records cannot enable automated sync, prune, or self-heal;
- the retained static Application is a regular parent-owned file.

The command does not read Kubernetes credentials, clone private repositories,
resolve remote branch tips, or apply manifests. Online validation is not
implemented yet, so invocations must pass `--offline`; omitting it fails
explicitly instead of misreporting a local-only run as online evidence. Policy
failures exit with code 2; tool/configuration failures exit with code 1.

## Ownership boundary

The root CLI owns extension discovery, built-in collision prevention, help,
completion, TTY inheritance, and exit-code propagation. `zed-gitops` owns the
current validation implementation. Follow-up work should expose the existing
`git_submodules` repository-identity and index primitives as a stable
`zed-pkg` library surface so the validator does not maintain parallel generic
Git parsing.

The deployment-specific application catalog and Argo policy remain versioned in
the target repository (`k8s-cluster` today). The gitlink allow-list is a
separate versioned schema the target repo owns; a fixture copy lives in
`src/bin/zed_gitops/testdata/`. Zed remains the validator UX rather than the
deployment controller.
