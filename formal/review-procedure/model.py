#!/usr/bin/env python3
"""Bounded install/frozen/publish model plus deterministic artifact checks.

Checks raise :class:`ModelViolation` (never bare ``assert``), so the model
still fails loudly under ``python3 -O`` — the same hardening the Sonus lineage
of this framework applies in its ``check.py``.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, replace
from hashlib import sha256
from itertools import permutations
from typing import NoReturn

NONE = -1
VERSIONS = (0, 1, 2)
MAX_DEPTH = 9

ALIASES = {
    "i": "install",
    "test-local": "r2g",
    "signin": "auth-login",
    "login": "auth-login",
    "signup": "auth-signup",
    "register": "auth-signup",
    "logout": "auth-logout",
    "signout": "auth-logout",
}


class ModelViolation(AssertionError):
    """Raised when the model reaches a state that breaks an invariant."""


def fail(message: str) -> NoReturn:
    raise ModelViolation(message)


def require(condition: bool, message: str) -> None:
    if not condition:
        fail(message)


@dataclass(frozen=True, slots=True)
class State:
    manifest: int = 0
    lock: int = NONE
    tree: int = NONE
    clean: bool = True
    tag: int = NONE
    published: int = NONE


def frozen_install(state: State) -> tuple[State, bool]:
    if state.lock == state.manifest and state.lock != NONE:
        return replace(state, tree=state.lock), True
    return state, False


def publish(state: State) -> tuple[State, bool]:
    allowed = (
        state.clean
        and state.tag == state.manifest
        and state.lock == state.manifest
        and state.tree == state.lock
    )
    return (replace(state, published=state.manifest), True) if allowed else (state, False)


def artifact_digest(entries: tuple[tuple[str, bytes], ...]) -> str:
    digest = sha256()
    for path, payload in sorted(entries, key=lambda item: item[0]):
        digest.update(path.encode("utf-8"))
        digest.update(b"\0")
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)
    return digest.hexdigest()


def canonical_command(command: str) -> str:
    return ALIASES.get(command, command)


def successors(state: State):
    for version in VERSIONS:
        if version != state.manifest:
            yield f"edit({version})", replace(
                state, manifest=version, clean=False, tag=NONE
            )
    yield "resolve", replace(state, lock=state.manifest)
    yield "install", replace(state, lock=state.manifest, tree=state.manifest)
    target, accepted = frozen_install(state)
    if accepted:
        yield "install-frozen", target
    if not state.clean:
        yield "mark-clean", replace(state, clean=True)
    yield "tag-head", replace(state, tag=state.manifest)
    target, accepted = publish(state)
    if accepted:
        yield "publish", target


def assert_invariants(state: State) -> None:
    require(state.manifest in VERSIONS, f"manifest out of domain: {state}")
    require(state.lock in (NONE, *VERSIONS), f"lock out of domain: {state}")
    require(state.tree in (NONE, *VERSIONS), f"tree out of domain: {state}")
    require(state.tag in (NONE, *VERSIONS), f"tag out of domain: {state}")
    require(state.published in (NONE, *VERSIONS), f"published out of domain: {state}")


def main() -> None:
    initial = State()
    queue = deque([(initial, 0)])
    seen = {initial}
    transitions = 0

    while queue:
        state, depth = queue.popleft()
        assert_invariants(state)

        frozen_target, frozen_ok = frozen_install(state)
        require(frozen_target.lock == state.lock, "frozen install mutated the lock")
        require(
            frozen_ok == (state.lock == state.manifest and state.lock != NONE),
            f"frozen-install admission diverged from the specification at {state}",
        )
        if not frozen_ok:
            require(frozen_target == state, "rejected frozen install mutated state")

        publish_target, publish_ok = publish(state)
        expected_publish = (
            state.clean
            and state.tag == state.manifest
            and state.lock == state.manifest
            and state.tree == state.lock
        )
        require(
            publish_ok == expected_publish,
            f"publish admission diverged from the specification at {state}",
        )
        if not publish_ok:
            require(publish_target == state, "rejected publish mutated state")

        if depth == MAX_DEPTH:
            continue
        for action, target in successors(state):
            transitions += 1
            assert_invariants(target)
            if action == "publish":
                require(expected_publish, f"publish fired while inadmissible at {state}")
            if target not in seen:
                seen.add(target)
                queue.append((target, depth + 1))

    entries = (
        ("bin/zed-helper", b"helper"),
        ("lib/schema.json", b"{}"),
        ("LICENSE", b"license"),
    )
    expected_digest = artifact_digest(entries)
    for order in permutations(entries):
        require(
            artifact_digest(order) == expected_digest,
            "artifact digest is entry-order dependent",
        )

    for alias, canonical in ALIASES.items():
        require(canonical_command(alias) == canonical, f"alias {alias!r} miscanonicalized")
        require(
            canonical_command(canonical) == canonical,
            f"canonical command {canonical!r} is not idempotent",
        )

    print(
        f"zed install/publish model: {len(seen)} states, "
        f"{transitions} transitions; all invariants hold"
    )


if __name__ == "__main__":
    try:
        main()
    except ModelViolation as error:
        raise SystemExit(f"zed install/publish model violation: {error}") from error
