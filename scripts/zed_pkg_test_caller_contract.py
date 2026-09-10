#!/usr/bin/env python3
"""Ratchet the zed-cli caller for the external zed-pkg-test smoke gate."""

from __future__ import annotations

import argparse
import re
import sys
import tomllib
from pathlib import Path

FULL_SHA = r"[0-9a-f]{40}"
CALL_RE = re.compile(
    rf"(?m)^\s*uses:\s*zed-pkg-test/zed-pkg-e2e/\.github/workflows/"
    rf"candidate-smoke\.yml@(?P<sha>{FULL_SHA})\s*$"
)
CANONICAL_INTERFACES_GIT = "https://github.com/zed-pkg/zed-interfaces.git"


class ContractViolation(AssertionError):
    """Raised when the production caller drifts from its least-privilege contract."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ContractViolation(message)


def audit_workflow(text: str) -> tuple[str, str]:
    require("pull_request:" in text, "caller must run on pull requests")
    require("push:" in text and "branches: [main]" in text, "caller must run on main")
    require("workflow_dispatch:" in text, "caller must support manual replay")
    require("pull_request_target:" not in text, "pull_request_target is forbidden")
    require(
        re.search(r"(?m)^permissions:\s*$\n\s{2}contents:\s*read\s*$", text)
        is not None,
        "caller must declare top-level contents: read",
    )
    require(
        re.search(r"(?m)^\s{2}[a-zA-Z0-9_-]+:\s*write\s*$", text) is None,
        "caller may not request write permissions",
    )
    require("${{ secrets." not in text, "caller may not read repository secrets")
    require("secrets: inherit" not in text, "caller may not inherit secrets")
    require("persist-credentials: true" not in text, "caller may not persist credentials")

    calls = list(CALL_RE.finditer(text))
    require(len(calls) == 1, "caller must invoke exactly one exact-pinned candidate workflow")
    harness_sha = calls[0].group("sha")

    expected_cli = "zed_cli_ref: ${{ github.event.pull_request.head.sha || github.sha }}"
    require(expected_cli in text, "caller must pass the exact PR head or main commit")

    harness_refs = re.findall(rf"(?m)^\s*harness_ref:\s*({FULL_SHA})\s*$", text)
    require(len(harness_refs) == 1, "caller must pass exactly one exact harness_ref")
    require(
        harness_refs[0] == harness_sha,
        "reusable-workflow pin and harness_ref must be the same commit",
    )

    interface_refs = re.findall(
        rf"(?m)^\s*zed_interfaces_ref:\s*({FULL_SHA})\s*$", text
    )
    require(
        len(interface_refs) == 1,
        "caller must pass exactly one exact zed_interfaces_ref",
    )

    require("cancel-in-progress: true" in text, "superseded candidate runs must cancel")
    require("secrets:" not in text, "caller job must not pass a secrets map")
    return harness_sha, interface_refs[0]


def audit_candidate_interface(root: Path, expected_revision: str) -> None:
    cargo_path = root / "Cargo.toml"
    lock_path = root / "Cargo.lock"
    require(cargo_path.is_file(), "Cargo.toml is missing")
    require(lock_path.is_file(), "Cargo.lock is missing")

    cargo = tomllib.loads(cargo_path.read_text(encoding="utf-8"))
    dependencies = cargo.get("dependencies")
    require(isinstance(dependencies, dict), "Cargo.toml [dependencies] is missing")
    interface = dependencies.get("zed-interfaces")
    require(
        isinstance(interface, dict),
        "Cargo.toml must declare zed-interfaces as an exact Git dependency",
    )
    require(
        interface.get("git") == CANONICAL_INTERFACES_GIT,
        "Cargo.toml zed-interfaces must use the canonical Git repository",
    )
    require(
        interface.get("rev") == expected_revision,
        "workflow zed_interfaces_ref must match Cargo.toml zed-interfaces rev",
    )

    lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    packages = lock.get("package", [])
    require(isinstance(packages, list), "Cargo.lock package entries are invalid")
    locked = [
        package
        for package in packages
        if isinstance(package, dict) and package.get("name") == "zed-interfaces"
    ]
    require(len(locked) == 1, "Cargo.lock must contain exactly one zed-interfaces package")
    source = locked[0].get("source")
    require(isinstance(source, str), "Cargo.lock zed-interfaces source is missing")
    expected_source = (
        f"git+{CANONICAL_INTERFACES_GIT}?rev={expected_revision}#{expected_revision}"
    )
    require(
        source == expected_source,
        "workflow zed_interfaces_ref must match Cargo.lock zed-interfaces source",
    )


def audit_documentation(text: str) -> None:
    required = (
        "Every `zed-cli` pull request and `main` commit",
        "exact harness",
        "exact CLI commit",
        "exact interface commit",
        "Cargo.toml and Cargo.lock",
        "read-only repository permissions",
        "receives no secrets",
        "root or transitive fixture dependency lacks an exact",
        "does not replace full candidate certification",
        "lifecycle,",
        "browser E2E,",
        "install-boundary workflows",
        "same candidate SHA",
        "owning Linear issue",
        "github.com/zed-pkg",
    )
    for phrase in required:
        require(phrase in text, f"caller documentation is missing: {phrase}")


def audit_repository(root: Path) -> tuple[str, str]:
    workflow = root / ".github/workflows/zed-pkg-test-candidate.yml"
    documentation = root / "docs/zed-pkg-test.md"
    require(workflow.is_file(), "zed-pkg-test caller workflow is missing")
    require(documentation.is_file(), "zed-pkg-test caller documentation is missing")
    harness_sha, interface_sha = audit_workflow(workflow.read_text(encoding="utf-8"))
    audit_candidate_interface(root, interface_sha)
    audit_documentation(documentation.read_text(encoding="utf-8"))
    return harness_sha, interface_sha


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        harness_sha, interface_sha = audit_repository(args.root.resolve())
    except (ContractViolation, tomllib.TOMLDecodeError) as error:
        print(f"zed-pkg-test caller contract failed: {error}", file=sys.stderr)
        return 1
    print(
        "zed-pkg-test caller contract passed at "
        f"harness {harness_sha} with zed-interfaces {interface_sha}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
