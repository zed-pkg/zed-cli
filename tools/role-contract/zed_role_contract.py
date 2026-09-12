#!/usr/bin/env python3
"""Audit and transactionally repair role-based Zed package dependencies.

Every expected dependency is derived from a producer ``.zpkg.toml`` in the
scanned cohort. ``fix`` performs comment-preserving manifest edits, refreshes
each affected lock with an explicit command, verifies the resulting graph, and
rolls the whole cohort back on any failure.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shlex
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable, Mapping

REQUIRED_ROLES: dict[str, tuple[str, ...]] = {
    "interfaces": (),
    "clients": ("interfaces",),
    "lib": ("interfaces",),
    "server": ("interfaces", "lib"),
    "cli": ("clients", "interfaces", "lib"),
    "mcp": ("clients", "interfaces", "lib"),
    "e2e": ("clients", "lib", "interfaces", "cli"),
}

ROLE_SUFFIXES: tuple[tuple[str, str], ...] = (
    ("-mcp-server.rs", "mcp"),
    ("-api-server.rs", "server"),
    ("-web-server.rs", "server"),
    ("-interfaces", "interfaces"),
    ("-clients", "clients"),
    ("-libs", "lib"),
    ("-lib", "lib"),
    ("-cli", "cli"),
    ("-e2e", "e2e"),
)

EDITABLE_CODES = frozenset({"DEPENDENCY_MISSING", "DEPENDENCY_CONSTRAINT_STALE"})
DEPENDENCIES_HEADER = re.compile(r"^\s*\[\s*dependencies\s*\]\s*(?:#.*)?(?:\r?\n)?$")
TABLE_HEADER = re.compile(r"^\s*\[\[?.*?\]?\]\s*(?:#.*)?(?:\r?\n)?$")
ASSIGNMENT = re.compile(
    r"^(?P<indent>\s*)"
    r"(?P<key>\"(?:[^\"\\]|\\.)*\"|'[^']*'|[A-Za-z0-9_.-]+)"
    r"(?P<separator>\s*=\s*)"
    r"(?P<value>\"(?:[^\"\\]|\\.)*\"|'[^']*')"
    r"(?P<suffix>\s*(?:#.*)?)"
    r"(?P<newline>\r?\n)?$"
)


@dataclass(frozen=True)
class Package:
    manifest: str
    org: str
    name: str
    version: str
    family: str | None
    role: str | None
    role_source: str
    dependencies: dict[str, str]

    @property
    def coordinate(self) -> str:
        return f"{self.org}/{self.name}"


@dataclass(frozen=True)
class Finding:
    code: str
    manifest: str
    message: str
    dependency: str | None = None


@dataclass(frozen=True)
class PlannedChange:
    manifest: str
    dependency: str
    previous: str | None
    expected: str


def infer_role(name: str) -> tuple[str | None, str | None]:
    for suffix, role in ROLE_SUFFIXES:
        if name.endswith(suffix) and len(name) > len(suffix):
            return role, name[: -len(suffix)]
    return None, None


def load_package(manifest: Path) -> tuple[Package | None, list[Finding]]:
    findings: list[Finding] = []
    try:
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        return None, [Finding("MANIFEST_INVALID", str(manifest), str(error))]

    package = data.get("package")
    if not isinstance(package, dict):
        return None, [Finding("PACKAGE_TABLE_MISSING", str(manifest), "missing [package] table")]

    org = package.get("org")
    name = package.get("name")
    version = package.get("version")
    if not all(isinstance(value, str) and value for value in (org, name, version)):
        return None, [Finding("PACKAGE_IDENTITY_INVALID", str(manifest), "package org, name, and version must be non-empty strings")]

    explicit_role = package.get("role")
    inferred_role, family = infer_role(name)
    if explicit_role is not None:
        if explicit_role not in REQUIRED_ROLES:
            findings.append(Finding("ROLE_INVALID", str(manifest), f"unsupported explicit package.role {explicit_role!r}"))
            role = None
            source = "explicit-invalid"
        else:
            role = explicit_role
            source = "explicit"
            family_value = package.get("family")
            if isinstance(family_value, str) and family_value:
                family = family_value
    else:
        role = inferred_role
        source = "heuristic" if role else "ambiguous"

    if role is None or family is None:
        findings.append(Finding("ROLE_AMBIGUOUS", str(manifest), "set package.role and package.family explicitly; naming did not identify one canonical role"))

    raw_dependencies = data.get("dependencies", {})
    if not isinstance(raw_dependencies, dict):
        findings.append(Finding("DEPENDENCIES_INVALID", str(manifest), "[dependencies] must be a table"))
        dependencies: dict[str, str] = {}
    else:
        dependencies = {key: value for key, value in raw_dependencies.items() if isinstance(key, str) and isinstance(value, str)}
        if len(dependencies) != len(raw_dependencies):
            findings.append(Finding("DEPENDENCY_VALUE_INVALID", str(manifest), "dependency identities and constraints must be strings"))

    return Package(str(manifest), org, name, version, family, role, source, dependencies), findings


def discover_manifests(roots: Iterable[Path]) -> list[Path]:
    manifests: set[Path] = set()
    for root in roots:
        if root.is_file() and root.name == ".zpkg.toml":
            manifests.add(root.resolve())
        elif root.is_dir():
            manifests.update(path.resolve() for path in root.rglob(".zpkg.toml"))
    return sorted(manifests)


def producer_index(packages: Iterable[Package]) -> tuple[dict[tuple[str, str, str], Package], list[Finding]]:
    producers: dict[tuple[str, str, str], Package] = {}
    findings: list[Finding] = []
    for package in packages:
        if package.role is None or package.family is None:
            continue
        key = (package.org, package.family, package.role)
        previous = producers.get(key)
        if previous:
            findings.append(Finding("ROLE_CONFLICT", package.manifest, f"role conflicts with {previous.coordinate} at {previous.manifest}"))
        else:
            producers[key] = package
    return producers, findings


def audit(packages: list[Package], initial: list[Finding]) -> list[Finding]:
    findings = list(initial)
    producers, role_findings = producer_index(packages)
    findings.extend(role_findings)

    for consumer in packages:
        if consumer.role is None or consumer.family is None:
            continue
        for required_role in REQUIRED_ROLES[consumer.role]:
            producer = producers.get((consumer.org, consumer.family, required_role))
            if producer is None:
                findings.append(Finding("PRODUCER_MISSING", consumer.manifest, f"no scanned producer supplies role {required_role!r} for family {consumer.family!r}"))
                continue
            actual = consumer.dependencies.get(producer.coordinate)
            expected = f"^{producer.version}"
            if actual is None:
                findings.append(Finding("DEPENDENCY_MISSING", consumer.manifest, f"requires scanned producer {producer.coordinate} at {expected}", producer.coordinate))
            elif actual != expected:
                findings.append(Finding("DEPENDENCY_CONSTRAINT_STALE", consumer.manifest, f"expected {expected!r} from producer manifest, found {actual!r}", producer.coordinate))

    coordinates = {package.coordinate for package in packages}
    graph = {package.coordinate: [dependency for dependency in package.dependencies if dependency in coordinates] for package in packages}
    visiting: set[str] = set()
    visited: set[str] = set()

    def visit(node: str, trail: list[str]) -> None:
        if node in visiting:
            cycle = trail[trail.index(node):] + [node]
            manifest = next(package.manifest for package in packages if package.coordinate == node)
            findings.append(Finding("DEPENDENCY_CYCLE", manifest, " -> ".join(cycle)))
            return
        if node in visited:
            return
        visiting.add(node)
        for neighbor in graph.get(node, []):
            visit(neighbor, trail + [neighbor])
        visiting.remove(node)
        visited.add(node)

    for node in sorted(graph):
        visit(node, [node])

    return sorted(findings, key=lambda item: (item.manifest, item.code, item.dependency or ""))


def load_cohort(roots: Iterable[Path]) -> tuple[list[Path], list[Package], list[Finding]]:
    manifests = discover_manifests(roots)
    packages: list[Package] = []
    findings: list[Finding] = []
    for manifest in manifests:
        package, package_findings = load_package(manifest)
        findings.extend(package_findings)
        if package:
            packages.append(package)
    if not manifests:
        findings.append(Finding("NO_MANIFESTS", "", "no .zpkg.toml manifests found"))
    return manifests, packages, audit(packages, findings)


def expected_dependency_updates(packages: list[Package], findings: Iterable[Finding]) -> tuple[dict[Path, dict[str, str]], list[PlannedChange]]:
    by_manifest = {Path(package.manifest): package for package in packages}
    producers, conflicts = producer_index(packages)
    if conflicts:
        raise ValueError("cannot compute dependency updates with conflicting producers")
    updates: dict[Path, dict[str, str]] = {}
    planned: list[PlannedChange] = []
    for finding in findings:
        if finding.code not in EDITABLE_CODES or not finding.dependency:
            continue
        manifest = Path(finding.manifest)
        consumer = by_manifest.get(manifest)
        if consumer is None or consumer.role is None or consumer.family is None:
            raise ValueError(f"cannot resolve consumer for {manifest}")
        producer = next((candidate for (org, family, _role), candidate in producers.items() if org == consumer.org and family == consumer.family and candidate.coordinate == finding.dependency), None)
        if producer is None:
            raise ValueError(f"dependency {finding.dependency} is not backed by a scanned producer")
        expected = f"^{producer.version}"
        updates.setdefault(manifest, {})[producer.coordinate] = expected
        planned.append(PlannedChange(str(manifest), producer.coordinate, consumer.dependencies.get(producer.coordinate), expected))
    return updates, sorted(planned, key=lambda item: (item.manifest, item.dependency))


def decode_key(value: str) -> str:
    if value.startswith('"'):
        try:
            decoded = json.loads(value)
        except json.JSONDecodeError as error:
            raise ValueError(f"invalid quoted dependency key {value!r}") from error
        if not isinstance(decoded, str):
            raise ValueError(f"dependency key is not a string: {value!r}")
        return decoded
    if value.startswith("'"):
        return value[1:-1]
    return value


def patch_manifest_text(text: str, updates: Mapping[str, str]) -> str:
    if not updates:
        return text
    newline = "\r\n" if "\r\n" in text else "\n"
    lines = text.splitlines(keepends=True)
    dependency_start: int | None = None
    for index, line in enumerate(lines):
        if DEPENDENCIES_HEADER.match(line):
            if dependency_start is not None:
                raise ValueError("multiple [dependencies] tables are not supported")
            dependency_start = index

    remaining = dict(sorted(updates.items()))
    if dependency_start is None:
        result = text
        if result and not result.endswith(("\n", "\r")):
            result += newline
        if result and not result.endswith(newline * 2):
            result += newline
        result += "[dependencies]" + newline
        for coordinate, expected in remaining.items():
            result += f"{json.dumps(coordinate)} = {json.dumps(expected)}{newline}"
    else:
        dependency_end = len(lines)
        for index in range(dependency_start + 1, len(lines)):
            if TABLE_HEADER.match(lines[index]):
                dependency_end = index
                break
        for index in range(dependency_start + 1, dependency_end):
            match = ASSIGNMENT.match(lines[index])
            if not match:
                continue
            coordinate = decode_key(match.group("key"))
            expected = remaining.pop(coordinate, None)
            if expected is None:
                continue
            line_ending = match.group("newline") or ""
            lines[index] = f"{match.group('indent')}{match.group('key')}{match.group('separator')}{json.dumps(expected)}{match.group('suffix')}{line_ending}"
        if remaining:
            if dependency_end > 0 and not lines[dependency_end - 1].endswith(("\n", "\r")):
                lines[dependency_end - 1] += newline
            lines[dependency_end:dependency_end] = [f"{json.dumps(coordinate)} = {json.dumps(expected)}{newline}" for coordinate, expected in remaining.items()]
        result = "".join(lines)

    try:
        parsed = tomllib.loads(result)
    except tomllib.TOMLDecodeError as error:
        raise ValueError(f"patched manifest is invalid TOML: {error}") from error
    dependencies = parsed.get("dependencies")
    if not isinstance(dependencies, dict):
        raise ValueError("patched manifest does not contain [dependencies]")
    for coordinate, expected in updates.items():
        if dependencies.get(coordinate) != expected:
            raise ValueError(f"patched manifest did not set {coordinate!r} to {expected!r}")
    return result


def atomic_write(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    temporary_path = Path(temporary)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(content)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary_path, path)
    finally:
        temporary_path.unlink(missing_ok=True)


def restore_snapshot(snapshot: Mapping[Path, bytes | None]) -> None:
    for path, content in snapshot.items():
        if content is None:
            path.unlink(missing_ok=True)
        else:
            atomic_write(path, content)


def run_lock_command(command_text: str, package_root: Path) -> None:
    command = shlex.split(command_text, posix=os.name != "nt")
    if not command:
        raise ValueError("--lock-command must not be empty")
    completed = subprocess.run(command, cwd=package_root, check=False)
    if completed.returncode != 0:
        raise RuntimeError(f"lock command failed with exit code {completed.returncode} in {package_root}")
    if not (package_root / ".zpkg.lock").is_file():
        raise RuntimeError(f"lock command succeeded but did not create {package_root / '.zpkg.lock'}")


def write_report(path: Path | None, report: dict[str, object]) -> None:
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if path:
        path.write_text(encoded, encoding="utf-8")
    else:
        sys.stdout.write(encoded)


def base_report(packages: list[Package], findings: list[Finding]) -> dict[str, object]:
    return {"schema": "zed.role-contract-audit/v1", "packages": [asdict(package) | {"coordinate": package.coordinate} for package in packages], "findings": [asdict(finding) for finding in findings], "ok": not findings}


def fix(roots: list[Path], *, output: Path | None, lock_command: str, dry_run: bool) -> int:
    _manifests, packages, findings = load_cohort(roots)
    report = base_report(packages, findings)
    blockers = [finding for finding in findings if finding.code not in EDITABLE_CODES]
    if blockers:
        report["fix"] = {"status": "blocked", "dry_run": dry_run, "changed_manifests": [], "lock_commands": [], "message": "non-editable findings must be resolved before fix can mutate files"}
        write_report(output, report)
        return 2

    updates, planned = expected_dependency_updates(packages, findings)
    if not updates:
        report["fix"] = {"status": "clean", "dry_run": dry_run, "changed_manifests": [], "lock_commands": []}
        write_report(output, report)
        return 0

    patched: dict[Path, bytes] = {}
    for manifest, manifest_updates in updates.items():
        original = manifest.read_text(encoding="utf-8")
        revised = patch_manifest_text(original, manifest_updates)
        if revised == original:
            raise RuntimeError(f"planned update did not change {manifest}")
        patched[manifest] = revised.encode("utf-8")

    changed_manifests = [str(path) for path in sorted(patched)]
    lock_commands = [{"cwd": str(path.parent), "command": lock_command} for path in sorted(patched)]
    if dry_run:
        report["fix"] = {"status": "planned", "dry_run": True, "planned_changes": [asdict(change) for change in planned], "changed_manifests": changed_manifests, "lock_commands": lock_commands}
        write_report(output, report)
        return 0

    snapshot: dict[Path, bytes | None] = {}
    for manifest in patched:
        lock = manifest.parent / ".zpkg.lock"
        snapshot[manifest] = manifest.read_bytes()
        snapshot[lock] = lock.read_bytes() if lock.exists() else None

    try:
        for manifest, content in patched.items():
            atomic_write(manifest, content)
        for manifest in sorted(patched):
            run_lock_command(lock_command, manifest.parent)
        _after_manifests, after_packages, after_findings = load_cohort(roots)
        if after_findings:
            messages = "; ".join(f"{finding.code}: {finding.message}" for finding in after_findings)
            raise RuntimeError(f"post-fix role graph is not clean: {messages}")
    except Exception as error:
        restore_snapshot(snapshot)
        _rollback_manifests, rollback_packages, rollback_findings = load_cohort(roots)
        rollback_report = base_report(rollback_packages, rollback_findings)
        rollback_report["fix"] = {"status": "rolled-back", "dry_run": False, "planned_changes": [asdict(change) for change in planned], "changed_manifests": changed_manifests, "lock_commands": lock_commands, "error": str(error)}
        write_report(output, rollback_report)
        return 2

    success_report = base_report(after_packages, after_findings)
    success_report["fix"] = {"status": "applied", "dry_run": False, "planned_changes": [asdict(change) for change in planned], "changed_manifests": changed_manifests, "lock_commands": lock_commands}
    write_report(output, success_report)
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("audit", "check", "fix"))
    parser.add_argument("roots", nargs="+", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--lock-command", default="zed install", help="command run in every changed package root to refresh .zpkg.lock")
    parser.add_argument("--dry-run", action="store_true", help="with fix, report exact edits and lock commands without writing")
    args = parser.parse_args(argv)

    if args.mode == "fix":
        return fix(args.roots, output=args.output, lock_command=args.lock_command, dry_run=args.dry_run)
    if args.dry_run:
        parser.error("--dry-run is only valid with fix")
    _manifests, packages, findings = load_cohort(args.roots)
    report = base_report(packages, findings)
    write_report(args.output, report)
    return 0 if args.mode == "audit" or not findings else 2


if __name__ == "__main__":
    raise SystemExit(main())
