#!/usr/bin/env python3
"""Ratchet the zed-cli caller for the external zed-pkg-test smoke gate."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

FULL_SHA = r"[0-9a-f]{40}"
CALL_RE = re.compile(
    rf"(?m)^\s*uses:\s*zed-pkg-test/zed-pkg-e2e/\.github/workflows/"
    rf"candidate-smoke\.yml@(?P<sha>{FULL_SHA})\s*$"
)


class ContractViolation(AssertionError):
    """Raised when the production caller drifts from its least-privilege contract."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ContractViolation(message)


def audit_workflow(text: str) -> str:
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

    require("cancel-in-progress: true" in text, "superseded candidate runs must cancel")
    require("secrets:" not in text, "caller job must not pass a secrets map")
    return harness_sha


def audit_documentation(text: str) -> None:
    required = (
        "Every `zed-cli` pull request and `main` commit",
        "exact harness",
        "exact CLI commit",
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


def audit_repository(root: Path) -> str:
    workflow = root / ".github/workflows/zed-pkg-test-candidate.yml"
    documentation = root / "docs/zed-pkg-test.md"
    require(workflow.is_file(), "zed-pkg-test caller workflow is missing")
    require(documentation.is_file(), "zed-pkg-test caller documentation is missing")
    harness_sha = audit_workflow(workflow.read_text(encoding="utf-8"))
    audit_documentation(documentation.read_text(encoding="utf-8"))
    return harness_sha


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        harness_sha = audit_repository(args.root.resolve())
    except ContractViolation as error:
        print(f"zed-pkg-test caller contract failed: {error}", file=sys.stderr)
        return 1
    print(f"zed-pkg-test caller contract passed at harness {harness_sha}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
