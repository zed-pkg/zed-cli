#!/usr/bin/env python3
"""Bounded install/frozen/publish model plus deterministic artifact checks."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass, replace
from hashlib import sha256
from itertools import permutations

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
    assert state.manifest in VERSIONS
    assert state.lock in (NONE, *VERSIONS)
    assert state.tree in (NONE, *VERSIONS)
    assert state.tag in (NONE, *VERSIONS)
    assert state.published in (NONE, *VERSIONS)


def main() -> None:
    initial = State()
    queue = deque([(initial, 0)])
    seen = {initial}
    transitions = 0

    while queue:
        state, depth = queue.popleft()
        assert_invariants(state)

        frozen_target, frozen_ok = frozen_install(state)
        assert frozen_target.lock == state.lock, "frozen install mutated the lock"
        assert frozen_ok == (state.lock == state.manifest and state.lock != NONE)
        if not frozen_ok:
            assert frozen_target == state

        publish_target, publish_ok = publish(state)
        expected_publish = (
            state.clean
            and state.tag == state.manifest
            and state.lock == state.manifest
            and state.tree == state.lock
        )
        assert publish_ok == expected_publish
        if not publish_ok:
            assert publish_target == state

        if depth == MAX_DEPTH:
            continue
        for action, target in successors(state):
            transitions += 1
            assert_invariants(target)
            if action == "publish":
                assert expected_publish
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
        assert artifact_digest(order) == expected_digest

    for alias, canonical in ALIASES.items():
        assert canonical_command(alias) == canonical
        assert canonical_command(canonical) == canonical

    print(
        f"zed install/publish model: {len(seen)} states, "
        f"{transitions} transitions; all invariants hold"
    )


if __name__ == "__main__":
    main()
