# Conformance

This directory is the repository-local executable evidence boundary for `zed-cli`. Conformance tests contracts; it does not become a second contract authority.

`check.sh` is the stable lifecycle entry point. It always performs the cheap structural boundary checks. In full mode it also delegates to one project-owned behavioral runner when present, preferring `conformance/run` and then `conformance/run.sh`. The structural-only mode is suitable for `post-install`; build, pack, publish, and pre-push use full mode.

A behavioral runner is optional while a repository has only boundary documentation. Once executable fixtures or compatibility cases exist, expose them through `run` or `run.sh` so Zed lifecycle and Git-hook gates cannot silently bypass them.
