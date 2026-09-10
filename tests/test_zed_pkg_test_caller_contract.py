from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
sys.path.insert(0, str(SCRIPTS))

import zed_pkg_test_caller_contract as contract  # noqa: E402

SHA = "a" * 40
OTHER_SHA = "b" * 40
INTERFACE_SHA = "c" * 40

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
      zed_interfaces_ref: {INTERFACE_SHA}
      harness_ref: {SHA}
'''

VALID_DOCS = '''
Every `zed-cli` pull request and `main` commit uses an exact harness, exact CLI commit, and exact interface commit.
The interface pin is checked against Cargo.toml and Cargo.lock before compilation.
It has read-only repository permissions, receives no secrets, and fails when a root or transitive fixture dependency lacks an exact commit.
The smoke gate does not replace full candidate certification: lifecycle, browser E2E, and install-boundary workflows use the same candidate SHA.
Record evidence on the owning Linear issue under github.com/zed-pkg.
'''

VALID_CARGO = f'''[package]
name = "zed-cli"
version = "0.3.0"

[dependencies]
zed-interfaces = {{ git = "{contract.CANONICAL_INTERFACES_GIT}", rev = "{INTERFACE_SHA}" }}
'''

VALID_LOCK = f'''version = 4

[[package]]
name = "zed-interfaces"
version = "0.1.0"
source = "git+{contract.CANONICAL_INTERFACES_GIT}?rev={INTERFACE_SHA}#{INTERFACE_SHA}"
'''


class CallerWorkflowTests(unittest.TestCase):
    def test_valid_caller_returns_the_harness_and_interface_pins(self) -> None:
        self.assertEqual(contract.audit_workflow(VALID_WORKFLOW), (SHA, INTERFACE_SHA))

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

    def test_mutable_interface_ref_is_rejected(self) -> None:
        with self.assertRaisesRegex(contract.ContractViolation, "zed_interfaces_ref"):
            contract.audit_workflow(
                VALID_WORKFLOW.replace(
                    f"zed_interfaces_ref: {INTERFACE_SHA}",
                    "zed_interfaces_ref: main",
                )
            )

    def test_duplicate_interface_ref_is_rejected(self) -> None:
        with self.assertRaisesRegex(contract.ContractViolation, "exactly one"):
            contract.audit_workflow(
                VALID_WORKFLOW + f"\n      zed_interfaces_ref: {OTHER_SHA}\n"
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


class CandidateInterfaceTests(unittest.TestCase):
    def write_candidate(self, cargo: str = VALID_CARGO, lock: str = VALID_LOCK) -> Path:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        (root / "Cargo.toml").write_text(cargo, encoding="utf-8")
        (root / "Cargo.lock").write_text(lock, encoding="utf-8")
        return root

    def test_matching_manifest_and_lock_pass(self) -> None:
        contract.audit_candidate_interface(self.write_candidate(), INTERFACE_SHA)

    def test_manifest_revision_mismatch_is_rejected(self) -> None:
        root = self.write_candidate(
            cargo=VALID_CARGO.replace(INTERFACE_SHA, OTHER_SHA)
        )
        with self.assertRaisesRegex(contract.ContractViolation, "Cargo.toml"):
            contract.audit_candidate_interface(root, INTERFACE_SHA)

    def test_lock_revision_mismatch_is_rejected(self) -> None:
        root = self.write_candidate(lock=VALID_LOCK.replace(INTERFACE_SHA, OTHER_SHA))
        with self.assertRaisesRegex(contract.ContractViolation, "Cargo.lock"):
            contract.audit_candidate_interface(root, INTERFACE_SHA)

    def test_noncanonical_interface_repository_is_rejected(self) -> None:
        root = self.write_candidate(
            cargo=VALID_CARGO.replace(
                contract.CANONICAL_INTERFACES_GIT,
                "https://example.invalid/zed-interfaces.git",
            )
        )
        with self.assertRaisesRegex(contract.ContractViolation, "canonical"):
            contract.audit_candidate_interface(root, INTERFACE_SHA)


class CallerDocumentationTests(unittest.TestCase):
    def test_valid_documentation_passes(self) -> None:
        contract.audit_documentation(VALID_DOCS)

    def test_missing_full_certification_boundary_is_rejected(self) -> None:
        with self.assertRaises(contract.ContractViolation):
            contract.audit_documentation(
                VALID_DOCS.replace("does not replace full candidate certification", "is sufficient")
            )

    def test_missing_interface_authority_is_rejected(self) -> None:
        with self.assertRaisesRegex(contract.ContractViolation, "exact interface commit"):
            contract.audit_documentation(
                VALID_DOCS.replace("exact interface commit", "interface dependency")
            )


if __name__ == "__main__":
    unittest.main()
