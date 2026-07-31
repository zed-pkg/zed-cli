# Formal review procedure: package resolution, frozen installs, content-addressed storage, aliases, and publication provenance

This document defines when formal evidence is required, which obligations a
change can affect, and what a pull request must record. It is additive: existing
model checkers, proof harnesses, property tests, fuzzers, and implementation
tests remain authoritative at their respective boundaries.

## Boundary

The procedure covers abstract manifest/lock/install/publish state and deterministic artifacts; remote registry availability, archive-library correctness, and VCS host behavior require separate integration tests.

The executable `formal/review-procedure/model.py` exhaustively explores a deliberately finite abstraction. It is an independent sentinel, not a full proof of the production implementation.

The machine-readable source of truth is
`formal/review-procedure/obligations.json`; CI validates its schema and runs the
bounded sentinel where this PR supplies one.

## Obligations

1. **ZED_FROZEN (Safety).** A frozen install uses exactly the lockfile or fails without mutating the install tree.
2. **ZED_STORE (Safety).** A content digest identifies immutable bytes and cannot be rebound to different content.
3. **ZED_PUBLISH (Safety).** Publish requires a clean tree and a matching version tag at the exact commit.
4. **ZED_ALIAS (Refinement).** Command aliases normalize to the canonical command before semantic dispatch.
5. **ZED_ARTIFACT (Refinement).** Equal source, manifest, lock, and toolchain inputs produce byte-identical artifacts.

Safety and liveness are reviewed separately. A liveness claim must name its
fairness, delivery, resource, and eventual-synchrony assumptions instead of
presenting progress as unconditional.

## When to update formal evidence

Update this procedure, the obligation register, and the strongest applicable
model when a PR changes any registered trigger path in a way that can alter:

- state variables, guards, ordering, retries, expiry, cancellation, or recovery;
- deterministic normalization or serialization;
- identity, ownership, threshold, quorum, or provenance decisions;
- persistence/snapshot fields that carry safety-relevant history; or
- an implementation function named by an existing refinement test.

A refactor may state “no abstract transition change” only when the PR explains
why and names deterministic tests that demonstrate observational equivalence.

## Required change sequence

1. **State the semantic delta.** Write the old and new transition, affected
   state, guard, and postcondition before implementation review.
2. **Select obligations.** List every obligation ID affected. Do not use a broad
   “formal methods passed” statement in place of specific claims.
3. **Update the model/register.** Add the smallest transition or obligation that
   captures the behavior. Bounds may not be weakened merely to remove a
   counterexample.
4. **Add production refinement tests.** Reproduce the abstract transition using
   real production code, deterministic scheduling/time, and explicit failure
   injection where applicable.
5. **Run and record evidence.** Include commands, results, bounds, assumptions,
   and any intentionally unproved surface in the PR.

## Baseline commands

```sh
python3 formal/review-procedure/check.py
python3 formal/review-procedure/model.py
```

Repository-specific refinement evidence includes:

- `cargo test --all-targets --locked`
- CLI alias and flags-2-env contract tests
- pack-twice byte comparison and throwaway registry/install roundtrip

## PR evidence block

```text
Formal surface:
Affected obligation IDs:
Old → new transition:
State/guard/postcondition:
Model or proof artifact:
Finite bound and assumptions:
Production refinement tests:
Commands and results:
Counterexample trace (when fixed):
Known unproved surface:
```

## Reviewer stop conditions

Block approval when an obligation is affected but absent from the evidence
block; a timeout/transport loss is treated as proof of failure; a bound is
weakened without justification; model and implementation tests disagree; a
state migration drops safety-relevant history; or a deterministic claim is
supported only by a probabilistic run.
