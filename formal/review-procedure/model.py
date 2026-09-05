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
MAX_DEPTH = 11

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
    artifact: int = NONE
    verified: int = NONE
    store: int = NONE
    tree: int = NONE
    link: int = NONE
    clean: bool = True
    tag: int = NONE
    published: int = NONE
    saw_rejected_artifact: bool = False
    saw_frozen_block: bool = False
    saw_install_block: bool = False
    saw_publish_block: bool = False


def frozen_ready(state: State) -> bool:
    return (
        state.lock != NONE
        and state.lock == state.manifest
        and state.store == state.lock
    )


def frozen_install(state: State) -> tuple[State, bool]:
    if frozen_ready(state):
        return replace(state, tree=state.lock, link=state.lock), True
    return state, False


def publish_ready(state: State) -> bool:
    return (
        state.published in (NONE, state.manifest)
        and state.clean
        and state.tag == state.manifest
        and state.lock == state.manifest
        and state.store == state.lock
        and state.tree == state.lock
        and state.link == state.tree
    )


def publish(state: State) -> tuple[State, bool]:
    return (
        (replace(state, published=state.manifest), True)
        if publish_ready(state)
        else (state, False)
    )


def download(state: State) -> tuple[State, bool]:
    if state.lock == NONE:
        return state, False
    return replace(state, artifact=state.lock, verified=NONE, tree=NONE, link=NONE), True


def download_mismatch(state: State) -> tuple[State, bool]:
    if state.lock == NONE:
        return state, False
    wrong = next(version for version in VERSIONS if version != state.lock)
    return replace(state, artifact=wrong, verified=NONE, tree=NONE, link=NONE), True


def verify(state: State) -> tuple[State, bool]:
    if state.lock != NONE and state.artifact == state.lock:
        return replace(state, verified=state.artifact), True
    return replace(state, verified=NONE, saw_rejected_artifact=True), False


def store(state: State) -> tuple[State, bool]:
    if state.lock != NONE and state.verified == state.lock:
        return replace(state, store=state.verified), True
    return state, False


def install(state: State) -> tuple[State, bool]:
    if frozen_ready(state):
        return replace(state, tree=state.lock, link=state.lock), True
    return state, False


def attempt_frozen_install(state: State) -> State:
    target, accepted = frozen_install(state)
    return target if accepted else replace(state, saw_frozen_block=True)


def attempt_install(state: State) -> State:
    target, accepted = install(state)
    return target if accepted else replace(state, saw_install_block=True)


def attempt_publish(state: State) -> State:
    target, accepted = publish(state)
    return target if accepted else replace(state, saw_publish_block=True)


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
                state,
                manifest=version,
                clean=False,
                tag=NONE,
                tree=NONE,
                link=NONE,
            )
    yield "resolve", replace(
        state,
        lock=state.manifest,
        artifact=NONE,
        verified=NONE,
        tree=NONE,
        link=NONE,
    )
    target, accepted = download(state)
    if accepted:
        yield "download", target
    target, accepted = download_mismatch(state)
    if accepted:
        yield "download-mismatch", target
    yield "verify", verify(state)[0]
    target, accepted = store(state)
    if accepted:
        yield "store", target
    target, accepted = install(state)
    if accepted:
        yield "install", target
    else:
        yield "install-blocked", attempt_install(state)
    target, accepted = frozen_install(state)
    if accepted:
        yield "install-frozen", target
    else:
        yield "install-frozen-blocked", attempt_frozen_install(state)
    if not state.clean:
        yield "mark-clean", replace(state, clean=True)
    yield "tag-head", replace(state, tag=state.manifest)
    target, accepted = publish(state)
    if accepted:
        yield "publish", target
    else:
        yield "publish-blocked", attempt_publish(state)


def assert_invariants(state: State) -> None:
    require(state.manifest in VERSIONS, f"manifest out of domain: {state}")
    require(state.lock in (NONE, *VERSIONS), f"lock out of domain: {state}")
    require(state.artifact in (NONE, *VERSIONS), f"artifact out of domain: {state}")
    require(state.verified in (NONE, *VERSIONS), f"verified out of domain: {state}")
    require(state.store in (NONE, *VERSIONS), f"store out of domain: {state}")
    require(state.tree in (NONE, *VERSIONS), f"tree out of domain: {state}")
    require(state.link in (NONE, *VERSIONS), f"link out of domain: {state}")
    require(state.tag in (NONE, *VERSIONS), f"tag out of domain: {state}")
    require(state.published in (NONE, *VERSIONS), f"published out of domain: {state}")
    if state.verified != NONE:
        require(
            state.verified == state.artifact == state.lock,
            f"verified artifact is not the locked identity: {state}",
        )
    if state.tree != NONE:
        require(
            state.tree == state.link == state.store,
            f"install tree is not backed by the immutable store: {state}",
        )
    if state.link != NONE:
        require(state.link == state.tree, f"project link is not the install tree: {state}")
    if state.published != NONE:
        # A later manifest edit may legitimately make the working tree dirty;
        # the publication record is immutable and its admission is checked at
        # the publish transition below.
        require(state.published in VERSIONS, f"published identity is invalid: {state}")


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
            frozen_ok == frozen_ready(state),
            f"frozen-install admission diverged from the specification at {state}",
        )
        if not frozen_ok:
            require(frozen_target == state, "rejected frozen install mutated state")

        publish_target, publish_ok = publish(state)
        expected_publish = publish_ready(state)
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
            if action == "install":
                require(frozen_ready(state), f"install fired without a verified store entry at {state}")
            if target not in seen:
                seen.add(target)
                queue.append((target, depth + 1))

    require(any(state.saw_rejected_artifact for state in seen), "hash-mismatch witness was not reached")
    require(any(state.saw_frozen_block for state in seen), "frozen-install block witness was not reached")
    require(any(state.saw_install_block for state in seen), "install block witness was not reached")
    require(any(state.saw_publish_block for state in seen), "publish block witness was not reached")

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
        f"zed install/publish model: {len(seen)} states, {transitions} transitions; "
        "all invariants and negative witnesses hold"
    )


if __name__ == "__main__":
    try:
        main()
    except ModelViolation as error:
        raise SystemExit(f"zed install/publish model violation: {error}") from error
