# Security policy

## Reporting a vulnerability

Use GitHub's private vulnerability-reporting or security-advisory flow for the
repository that contains `zed-lock`. While the crate is validated in the
`zed-pkg/zed-cli` workspace, report against `zed-pkg/zed-cli`; after the
history-preserving repository split, report against `zed-pkg/zed-lock`.

Do not open a public issue containing an unpatched lock-bypass, symlink or
reparse-point attack, descriptor/handle-inheritance flaw, fencing-token bypass,
or denial-of-service reproduction that could affect users.

A useful report includes:

- operating system, filesystem, and whether storage is local, NFS, SMB, or a
  container/shared-volume abstraction;
- the canonical and alias paths involved;
- process and thread topology;
- whether the failure involves blocking, timeout, cancellation, owner death,
  descriptor inheritance, or lock-file replacement;
- a minimal deterministic reproduction and the observed ownership overlap.

## Security model

- The operating-system descriptor or handle lock is authoritative. Lock-file
  contents, PID metadata, timestamps, and process names are diagnostics only.
- Lock files are stable rendezvous objects and must not be deleted or replaced
  to recover from an apparent stale owner.
- Lock directories must be private to the current user and protected from
  untrusted symlink or reparse-point substitution.
- Local locks do not make remote or network filesystems safe when their native
  locking semantics are unsupported or weakened. Use an explicitly supported
  filesystem or an outer Fiducia lease with fencing for genuinely multi-host
  shared state.
- A distributed notification is only a wake-up hint. Ownership begins only
  after authoritative acquisition returns a fresh fencing token.

## Supported releases

Until the standalone repository and first tagged release exist, security fixes
are delivered through the current `zed-cli` main branch and the in-tree
`crates/zed-lock` package. The eventual standalone repository should publish a
normal supported-version table with its first release.
