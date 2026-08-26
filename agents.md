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

## Code style and coding patterns

remember to modularize the rust, typescript and dart - not everything belongs in main.rs, main.ts and main.dart; also follow functional coding principles - fewer side-effects (use pure functions more), more immutability (immutable variables); but for stateful apps like the client or stateful servers like websockets or tcp connections, sometimes classes and oop make more sense than functional programming perse, but we can still adhere to functional programming more than usual. Favor exhaustive pattern matching and use formal methods checking too. Favor composability and re-use , so basically create more utility functions and routines for shared use. You can follow a medium level of D.R.Y. (don't repeat yourself) - in other words you can repeat yourself at medium amount (not too much not too little). Some chaining is totally fine, so either method-chaining (immutable sometimes although with classes can be mutable too for performance), and chaining via the pipe operator is ok in languages like gleamlang.

Functional programming is mostly the following:

+ explicit inputs
+ explicit outputs
+ immutable values
+ pure transformations
+ typed errors
+ explicit state transitions
+ composition
+ effects pushed outward
+ illegal states excluded by types
