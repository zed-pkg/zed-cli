# Agent instructions

## Scope and hierarchy

- These instructions apply to the whole `zed-pkg/zed-cli` repository unless a deeper lowercase `agents.md` adds narrower rules.
- Before editing, resolve the current working directory and load every readable ancestor `agents.md` from the filesystem root to the working directory. Do not search siblings. Resolve symlinks, deduplicate resolved files, and report unreadable or cyclic instruction files.
- `.claude/CLAUDE.md`, `.gemini/GEMINI.md`, and `.openai/AGENTS.md` are pointers only. Never duplicate instructions in tool-specific files.

## Repository role

This repository implements the `zed` package-manager CLI: manifest and lockfile resolution, installation ownership modes, adapters, build and publish flows, authentication, completions, self-update, and package-aware development shells.

## Working rules

- Preserve deterministic lockfile restoration and immutable store/build-cache inputs.
- Treat symlink and copy install modes as explicit ownership contracts; never silently introduce shared mutable inodes at container or deployment boundaries.
- Keep Clap options, flags-2-env mappings, help text, completions, and documented environment variables synchronized.
- Preserve exit codes, non-interactive behavior, stderr diagnostics, and protocol-safe stdout for automation.
- Redact tokens and bounded error context; never copy credentials or implicitly load production dotenv files.
- Reuse shared interfaces instead of redefining manifest, registry, version, or lockfile models locally.
- Add failure-mode and cross-platform tests for parser, filesystem, shell, registry, and process-boundary changes.
- Run the pinned formatting, nextest, doctest, Clippy, polyglot, manifestless, Docker/OCI, and develop-shell checks relevant to the change.

## Validation

The pinned `agents policy` workflow validates this hierarchy and the three tool pointers. Follow `README.md` and existing workflows for focused repository validation before requesting review.
