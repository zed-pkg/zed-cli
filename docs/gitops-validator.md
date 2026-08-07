# `zed-gitops` external validator

`zed-gitops` is the first read-only implementation slice for DEN-2725. It
validates the `GitOpsApplication` records owned by
`ORESoftware/k8s-cluster` without initializing or executing child repositories.

The executable is intentionally separate from the core package-manager command
graph. Once generic external-command discovery lands, the public spelling is:

```console
zed gitops validate --root . --offline --strict
```

The executable available in this PR is directly invokable as:

```console
zed-gitops validate --root . --offline --strict
zed-gitops validate --root . --offline --strict --format json
zed-gitops validate --root . --offline --strict --format sarif
```

## Evidence checked

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
resolve remote branch tips, or apply manifests. Policy failures exit with code
2; tool/configuration failures exit with code 1.

## Ownership boundary

This first slice keeps parsing local to the external executable so it can land
without changing ordinary install/update behavior or contending with unrelated
root-command branches. Follow-up work should expose the existing
`git_submodules` repository-identity and index primitives as a stable library
surface, then make core `zed` discover `zed-*` executables and route
`zed gitops ...` with the same help, completion, TTY, and exit-code behavior.

The deployment-specific schema and policy remain versioned in
`k8s-cluster`; Zed remains the validator UX rather than the deployment
controller.
