from __future__ import annotations

import sys
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))

import zed_pkg_test_caller_contract as contract  # noqa: E402

SHA = "a" * 40
OTHER_SHA = "b" * 40

VALID_WORKFLOW = f'''name: zed-pkg-test candidate smoke
on:
  pull_request:
  push:
    branches: [main]
  workflow_dispatch:
permissions:
  contents: read
concurrency:
  cancel-in-progress: true
jobs:
  candidate-smoke:
    uses: zed-pkg-test/zed-pkg-e2e/.github/workflows/candidate-smoke.yml@{SHA}
    with:
      zed_cli_ref: ${{{{ github.event.pull_request.head.sha || github.sha }}}}
      harness_ref: {SHA}
'''

VALID_DOCS = '''
Every `zed-cli` pull request and `main` commit uses an exact harness and exact CLI commit.
It has read-only repository permissions, receives no secrets, and fails when a root or transitive fixture dependency lacks an exact commit.
The smoke gate does not replace full candidate certification: lifecycle, browser E2E, and install-boundary workflows use the same candidate SHA.
Record evidence on the owning Linear issue under github.com/zed-pkg.
'''


class CallerWorkflowTests(unittest.TestCase):
    def test_valid_caller_returns_the_harness_pin(self) -> None:
        self.assertEqual(contract.audit_workflow(VALID_WORKFLOW), SHA)

    def test_mutable_workflow_ref_is_rejected(self) -> None:
        with self.assertRaisesRegex(contract.ContractViolation, "exact-pinned"):
            contract.audit_workflow(
                VALID_WORKFLOW.replace(f"candidate-smoke.yml@{SHA}", "candidate-smoke.yml@main")
            )

    def test_mismatched_harness_ref_is_rejected(self) -> None:
        with self.assertRaisesRegex(contract.ContractViolation, "same commit"):
            contract.audit_workflow(
                VALID_WORKFLOW.replace(f"harness_ref: {SHA}", f"harness_ref: {OTHER_SHA}")
            )

    def test_merge_commit_expression_is_rejected(self) -> None:
        with self.assertRaisesRegex(contract.ContractViolation, "exact PR head"):
            contract.audit_workflow(
                VALID_WORKFLOW.replace(
                    "github.event.pull_request.head.sha || github.sha", "github.sha"
                )
            )

    def test_write_permission_is_rejected(self) -> None:
        with self.assertRaisesRegex(contract.ContractViolation, "contents: read"):
            contract.audit_workflow(
                VALID_WORKFLOW.replace("contents: read", "contents: write")
            )

    def test_secret_inheritance_is_rejected(self) -> None:
        with self.assertRaisesRegex(contract.ContractViolation, "secrets"):
            contract.audit_workflow(VALID_WORKFLOW + "\nsecrets: inherit\n")

    def test_duplicate_remote_calls_are_rejected(self) -> None:
        duplicate = VALID_WORKFLOW + f'''\n  another:\n    uses: zed-pkg-test/zed-pkg-e2e/.github/workflows/candidate-smoke.yml@{SHA}\n'''
        with self.assertRaisesRegex(contract.ContractViolation, "exactly one"):
            contract.audit_workflow(duplicate)


class CallerDocumentationTests(unittest.TestCase):
    def test_valid_documentation_passes(self) -> None:
        contract.audit_documentation(VALID_DOCS)

    def test_missing_full_certification_boundary_is_rejected(self) -> None:
        with self.assertRaises(contract.ContractViolation):
            contract.audit_documentation(
                VALID_DOCS.replace("does not replace full candidate certification", "is sufficient")
            )


if __name__ == "__main__":
    unittest.main()
