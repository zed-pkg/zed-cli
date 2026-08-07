from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "audit-github-actions.py"
SPEC = importlib.util.spec_from_file_location("workflow_policy", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
workflow_policy = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(workflow_policy)

PINNED_CHECKOUT = "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"
PINNED_REUSABLE = (
    "zed-pkg/zed-monorepo/.github/workflows/agents-policy-reusable.yml"
    "@fb2417b1a976459e3de740f788916d3d91d3669e"
)


def good_workflow(extra: str = "") -> str:
    return f"""name: policy fixture

on:
  pull_request:

permissions:
  contents: read

concurrency:
  group: policy-${{{{ github.ref }}}}
  cancel-in-progress: true

jobs:
  test:
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: {PINNED_CHECKOUT}
        with:
          persist-credentials: false
      - name: Test
        run: python3 -V
{extra}"""


def privileged_release_workflow() -> str:
    return f"""name: release fixture

on:
  push:
    tags: [\"v*\"]

permissions:
  contents: read

concurrency:
  group: release-${{{{ github.ref }}}}
  cancel-in-progress: true

jobs:
  build:
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: {PINNED_CHECKOUT}
        with:
          persist-credentials: false
  release:
    needs: build
    runs-on: ubuntu-latest
    timeout-minutes: 10
    permissions:
      contents: write
    steps:
      - run: echo release
"""


class WorkflowPolicyTests(unittest.TestCase):
    def test_hardened_workflow_passes(self) -> None:
        self.assertEqual([], workflow_policy.audit_workflow_text("good.yml", good_workflow()))

    def test_mutable_action_ref_fails(self) -> None:
        findings = workflow_policy.audit_workflow_text(
            "mutable.yml", good_workflow().replace(PINNED_CHECKOUT, "actions/checkout@v4")
        )
        self.assertTrue(any("full 40-character commit SHA" in finding for finding in findings))

    def test_checkout_credentials_must_be_disabled(self) -> None:
        findings = workflow_policy.audit_workflow_text(
            "credentials.yml", good_workflow().replace("persist-credentials: false", "fetch-depth: 1")
        )
        self.assertTrue(any("persist-credentials: false" in finding for finding in findings))

    def test_top_level_write_permissions_fail(self) -> None:
        findings = workflow_policy.audit_workflow_text(
            "write.yml", good_workflow().replace("contents: read", "contents: write")
        )
        self.assertTrue(any("top-level write permissions" in finding for finding in findings))

    def test_top_level_inline_permission_map_fails_closed(self) -> None:
        workflow = good_workflow().replace(
            "permissions:\n  contents: read",
            "permissions: {contents: write}",
        )
        findings = workflow_policy.audit_workflow_text("inline-top.yml", workflow)
        self.assertTrue(any("canonical block mapping" in finding for finding in findings))

    def test_top_level_quoted_write_fails_closed(self) -> None:
        workflow = good_workflow().replace("contents: read", 'contents: "write"')
        findings = workflow_policy.audit_workflow_text("quoted-top.yml", workflow)
        self.assertTrue(any("unquoted" in finding for finding in findings))

    def test_job_write_requires_exact_allowance(self) -> None:
        workflow = privileged_release_workflow()
        findings = workflow_policy.audit_workflow_text("release.yml", workflow)
        self.assertTrue(any("unapproved contents: write" in finding for finding in findings))

        self.assertEqual(
            [],
            workflow_policy.audit_workflow_text(
                "release.yml",
                workflow,
                {"release": {"contents"}},
            ),
        )

    def test_job_write_all_fails_closed(self) -> None:
        workflow = privileged_release_workflow().replace(
            "    permissions:\n      contents: write",
            "    permissions: write-all",
        )
        findings = workflow_policy.audit_workflow_text("write-all.yml", workflow)
        self.assertTrue(any("canonical block mapping" in finding for finding in findings))

    def test_job_inline_write_map_fails_closed(self) -> None:
        workflow = privileged_release_workflow().replace(
            "    permissions:\n      contents: write",
            "    permissions: {contents: write}",
        )
        findings = workflow_policy.audit_workflow_text("inline-job.yml", workflow)
        self.assertTrue(any("canonical block mapping" in finding for finding in findings))

    def test_job_quoted_write_fails_closed(self) -> None:
        workflow = privileged_release_workflow().replace(
            "      contents: write",
            '      contents: "write"',
        )
        findings = workflow_policy.audit_workflow_text("quoted-job.yml", workflow)
        self.assertTrue(any("unquoted" in finding for finding in findings))

    def test_job_permission_alias_fails_closed(self) -> None:
        workflow = privileged_release_workflow().replace(
            "    permissions:\n      contents: write",
            "    permissions: *release_permissions",
        )
        findings = workflow_policy.audit_workflow_text("alias-job.yml", workflow)
        self.assertTrue(any("canonical block mapping" in finding for finding in findings))

    def test_stale_job_write_allowance_fails(self) -> None:
        findings = workflow_policy.audit_workflow_text(
            "good.yml",
            good_workflow(),
            {"release": {"contents"}},
        )
        self.assertTrue(any("stale privilege allowance" in finding for finding in findings))

    def test_pull_request_target_fails(self) -> None:
        findings = workflow_policy.audit_workflow_text(
            "target.yml", good_workflow().replace("pull_request:", "pull_request_target:")
        )
        self.assertTrue(any("pull_request_target is prohibited" in finding for finding in findings))

    def test_every_normal_job_requires_a_timeout(self) -> None:
        findings = workflow_policy.audit_workflow_text(
            "timeout.yml", good_workflow().replace("    timeout-minutes: 10\n", "")
        )
        self.assertTrue(any("must set timeout-minutes" in finding for finding in findings))

    def test_reusable_workflow_job_does_not_accept_timeout(self) -> None:
        workflow = f"""name: reusable fixture

on:
  pull_request:

permissions:
  contents: read

concurrency:
  group: reusable-${{{{ github.ref }}}}
  cancel-in-progress: true

jobs:
  validate:
    uses: {PINNED_REUSABLE}
"""
        self.assertEqual([], workflow_policy.audit_workflow_text("reusable.yml", workflow))

    def test_mutable_docker_action_fails(self) -> None:
        workflow = good_workflow(
            "      - uses: docker://example.invalid/tool:latest\n"
        )
        findings = workflow_policy.audit_workflow_text("docker.yml", workflow)
        self.assertTrue(any("sha256 digest" in finding for finding in findings))

    def test_exact_legacy_blob_is_accepted_but_changed_blob_is_audited(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            workflow_dir = root / ".github" / "workflows"
            workflow_dir.mkdir(parents=True)
            legacy = workflow_dir / "legacy.yml"
            legacy.write_text("name: legacy\non: [push]\njobs: {}\n", encoding="utf-8")
            baseline_path = root / ".github" / "workflow-security-baseline.json"
            baseline_path.write_text(
                json.dumps(
                    {
                        "version": 1,
                        "legacy_workflow_blobs": {
                            ".github/workflows/legacy.yml": workflow_policy.git_blob_sha(legacy.read_bytes())
                        },
                        "allowed_job_write_permissions": {},
                    }
                ),
                encoding="utf-8",
            )
            self.assertEqual([], workflow_policy.audit_repository(root, baseline_path))

            legacy.write_text("name: changed\non: [push]\njobs: {}\n", encoding="utf-8")
            findings = workflow_policy.audit_repository(root, baseline_path)
            self.assertTrue(any("legacy workflow changed" in finding for finding in findings))


if __name__ == "__main__":
    unittest.main()
