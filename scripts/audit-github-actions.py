#!/usr/bin/env python3
"""Fail closed on new or modified GitHub Actions workflow risk.

Legacy workflows are admitted only when their exact Git blob SHA matches the
reviewed baseline. Any new or changed workflow must satisfy the hardened policy.
Narrow job-level write grants require an explicit, path-and-job-specific policy
entry; top-level write grants are always rejected.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path
from typing import Iterable

FULL_SHA_RE = re.compile(r"^[0-9a-f]{40}$")
DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
JOB_RE = re.compile(r"^  ([A-Za-z0-9_-]+):(?:\s*#.*)?$")
USES_RE = re.compile(r"\buses:\s*([^\s#]+)")
PERMISSION_WRITE_RE = re.compile(r"^      ([A-Za-z0-9_-]+):\s*write\s*(?:#.*)?$")


def git_blob_sha(data: bytes) -> str:
    header = f"blob {len(data)}\0".encode("ascii")
    return hashlib.sha1(header + data).hexdigest()  # noqa: S324 - Git object identity


def top_level_section(lines: list[str], name: str) -> list[str] | None:
    marker = f"{name}:"
    for index, line in enumerate(lines):
        if line.rstrip() != marker:
            continue
        section: list[str] = []
        for candidate in lines[index + 1 :]:
            if candidate.strip() and not candidate.startswith((" ", "\t")):
                break
            section.append(candidate)
        return section
    return None


def workflow_jobs(lines: list[str]) -> list[tuple[str, list[str]]]:
    try:
        jobs_index = next(i for i, line in enumerate(lines) if line.rstrip() == "jobs:")
    except StopIteration:
        return []

    jobs: list[tuple[str, list[str]]] = []
    current_name: str | None = None
    current_lines: list[str] = []
    for line in lines[jobs_index + 1 :]:
        if line.strip() and not line.startswith((" ", "\t")):
            break
        match = JOB_RE.match(line.rstrip())
        if match:
            if current_name is not None:
                jobs.append((current_name, current_lines))
            current_name = match.group(1)
            current_lines = []
        elif current_name is not None:
            current_lines.append(line)
    if current_name is not None:
        jobs.append((current_name, current_lines))
    return jobs


def job_permission_writes(job_lines: list[str]) -> set[str]:
    writes: set[str] = set()
    in_permissions = False
    for line in job_lines:
        if line.rstrip() == "    permissions:":
            in_permissions = True
            continue
        if in_permissions and line.strip() and len(line) - len(line.lstrip(" ")) <= 4:
            in_permissions = False
        if not in_permissions:
            continue
        match = PERMISSION_WRITE_RE.match(line)
        if match:
            writes.add(match.group(1))
    return writes


def action_reference_finding(path: str, reference: str) -> str | None:
    if reference.startswith("./"):
        return None
    if reference.startswith("docker://"):
        image = reference.removeprefix("docker://")
        if "@" not in image or not DIGEST_RE.fullmatch(image.rsplit("@", 1)[1]):
            return f"{path}: docker action must be pinned by sha256 digest: {reference}"
        return None
    if "@" not in reference:
        return f"{path}: action is missing an immutable ref: {reference}"
    ref = reference.rsplit("@", 1)[1]
    if not FULL_SHA_RE.fullmatch(ref):
        return f"{path}: action must use a full 40-character commit SHA: {reference}"
    return None


def audit_checkout_credentials(path: str, lines: list[str]) -> list[str]:
    findings: list[str] = []
    for index, line in enumerate(lines):
        match = USES_RE.search(line)
        if not match or not match.group(1).startswith("actions/checkout@"):
            continue

        indentation = len(line) - len(line.lstrip(" "))
        block: list[str] = []
        for candidate in lines[index + 1 :]:
            candidate_indent = len(candidate) - len(candidate.lstrip(" "))
            if candidate.strip() and candidate_indent == indentation and candidate.lstrip().startswith("- "):
                break
            if candidate.strip() and candidate_indent < indentation:
                break
            block.append(candidate)
        if not any(re.match(r"^\s*persist-credentials:\s*false\s*(?:#.*)?$", item) for item in block):
            findings.append(
                f"{path}:{index + 1}: actions/checkout must set persist-credentials: false"
            )
    return findings


def audit_workflow_text(
    path: str,
    text: str,
    allowed_job_write_permissions: dict[str, set[str]] | None = None,
) -> list[str]:
    findings: list[str] = []
    lines = text.splitlines()
    allowed = allowed_job_write_permissions or {}
    observed_allowed: dict[str, set[str]] = {job: set() for job in allowed}

    if re.search(r"(?m)^\s*pull_request_target\s*:", text):
        findings.append(f"{path}: pull_request_target is prohibited")

    permissions = top_level_section(lines, "permissions")
    if permissions is None:
        findings.append(f"{path}: top-level permissions block is required")
    else:
        permission_text = "\n".join(permissions)
        if not re.search(r"(?m)^  contents:\s*read\s*(?:#.*)?$", permission_text):
            findings.append(f"{path}: permissions must include contents: read")
        if re.search(r"(?m)^  [A-Za-z0-9_-]+:\s*write\s*(?:#.*)?$", permission_text):
            findings.append(f"{path}: top-level write permissions are prohibited")

    concurrency = top_level_section(lines, "concurrency")
    if concurrency is None:
        findings.append(f"{path}: top-level concurrency block is required")
    elif not any(re.match(r"^  cancel-in-progress:\s*true\s*(?:#.*)?$", line) for line in concurrency):
        findings.append(f"{path}: concurrency must set cancel-in-progress: true")

    jobs = workflow_jobs(lines)
    if not jobs:
        findings.append(f"{path}: workflow must declare at least one job")
    for job_name, job_lines in jobs:
        reusable_job = any(re.match(r"^    uses:\s*\S+", line) for line in job_lines)
        has_timeout = any(
            re.match(r"^    timeout-minutes:\s*[1-9][0-9]*\s*(?:#.*)?$", line)
            for line in job_lines
        )
        if not reusable_job and not has_timeout:
            findings.append(f"{path}: job {job_name!r} must set timeout-minutes")

        writes = job_permission_writes(job_lines)
        allowed_writes = allowed.get(job_name, set())
        for permission in sorted(writes):
            if permission not in allowed_writes:
                findings.append(
                    f"{path}: job {job_name!r} has unapproved {permission}: write permission"
                )
            else:
                observed_allowed.setdefault(job_name, set()).add(permission)

    for job_name, permissions_for_job in sorted(allowed.items()):
        for permission in sorted(permissions_for_job - observed_allowed.get(job_name, set())):
            findings.append(
                f"{path}: stale privilege allowance for job {job_name!r}: {permission}: write"
            )

    for line_number, line in enumerate(lines, 1):
        if line.lstrip().startswith("#"):
            continue
        match = USES_RE.search(line)
        if not match:
            continue
        finding = action_reference_finding(path, match.group(1))
        if finding:
            findings.append(f"{finding} (line {line_number})")

    findings.extend(audit_checkout_credentials(path, lines))
    return findings


def iter_workflows(root: Path) -> Iterable[Path]:
    workflow_dir = root / ".github" / "workflows"
    yield from sorted((*workflow_dir.glob("*.yml"), *workflow_dir.glob("*.yaml")))


def load_policy(path: Path) -> tuple[dict[str, str], dict[str, dict[str, set[str]]]]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if payload.get("version") != 1:
        raise ValueError("workflow security baseline version must be 1")

    entries = payload.get("legacy_workflow_blobs")
    if not isinstance(entries, dict):
        raise ValueError("legacy_workflow_blobs must be an object")
    for workflow, blob_sha in entries.items():
        if not isinstance(workflow, str) or not isinstance(blob_sha, str) or not FULL_SHA_RE.fullmatch(blob_sha):
            raise ValueError(f"invalid workflow baseline entry: {workflow!r}: {blob_sha!r}")

    raw_allowed = payload.get("allowed_job_write_permissions", {})
    if not isinstance(raw_allowed, dict):
        raise ValueError("allowed_job_write_permissions must be an object")
    allowed: dict[str, dict[str, set[str]]] = {}
    for workflow, jobs in raw_allowed.items():
        if not isinstance(workflow, str) or not isinstance(jobs, dict):
            raise ValueError(f"invalid privileged workflow entry: {workflow!r}")
        allowed[workflow] = {}
        for job, permissions in jobs.items():
            if not isinstance(job, str) or not isinstance(permissions, list) or not permissions:
                raise ValueError(f"invalid privileged job entry: {workflow!r}: {job!r}")
            if any(not isinstance(permission, str) or not permission for permission in permissions):
                raise ValueError(f"invalid permission list: {workflow!r}: {job!r}")
            allowed[workflow][job] = set(permissions)
    return entries, allowed


def audit_repository(root: Path, baseline_path: Path) -> list[str]:
    baseline, allowed_writes = load_policy(baseline_path)
    findings: list[str] = []
    seen: set[str] = set()

    for workflow in iter_workflows(root):
        relative = workflow.relative_to(root).as_posix()
        seen.add(relative)
        data = workflow.read_bytes()
        current_blob = git_blob_sha(data)
        if baseline.get(relative) == current_blob:
            print(f"legacy workflow unchanged and baseline-locked: {relative} ({current_blob})")
            continue

        workflow_findings = audit_workflow_text(
            relative,
            data.decode("utf-8"),
            allowed_writes.get(relative),
        )
        if workflow_findings:
            if relative in baseline:
                workflow_findings.insert(
                    0,
                    f"{relative}: legacy workflow changed from reviewed blob {baseline[relative]} and is not fully hardened",
                )
            findings.extend(workflow_findings)
        else:
            print(f"hardened workflow accepted: {relative} ({current_blob})")

    for relative in sorted(set(baseline) - seen):
        findings.append(f"{relative}: stale baseline entry; workflow no longer exists")
    for relative in sorted(set(allowed_writes) - seen):
        findings.append(f"{relative}: stale privileged-workflow policy entry")
    return findings


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument(
        "--baseline",
        type=Path,
        default=Path(__file__).resolve().parents[1] / ".github" / "workflow-security-baseline.json",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        findings = audit_repository(args.root.resolve(), args.baseline.resolve())
    except (OSError, ValueError, json.JSONDecodeError, UnicodeDecodeError) as error:
        print(f"workflow policy audit failed to run: {error}", file=sys.stderr)
        return 2

    if findings:
        print("GitHub Actions policy violations:", file=sys.stderr)
        for finding in findings:
            print(f"- {finding}", file=sys.stderr)
        return 1

    print("GitHub Actions policy audit passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
