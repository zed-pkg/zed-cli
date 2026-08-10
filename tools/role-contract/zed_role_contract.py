#!/usr/bin/env python3
"""Fail-closed auditor for role-based Zed package dependencies.

This first delivery slice intentionally implements non-mutating inventory, audit,
and check modes. It never invents package coordinates: every expected dependency
must be backed by a producer manifest in the scanned cohort.
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Iterable

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
            if family is None:
                family_value = package.get("family")
                family = family_value if isinstance(family_value, str) and family_value else None
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


def audit(packages: list[Package], initial: list[Finding]) -> list[Finding]:
    findings = list(initial)
    producers: dict[tuple[str, str, str], Package] = {}

    for package in packages:
        if package.role is None or package.family is None:
            continue
        key = (package.org, package.family, package.role)
        previous = producers.get(key)
        if previous:
            findings.append(Finding("ROLE_CONFLICT", package.manifest, f"role conflicts with {previous.coordinate} at {previous.manifest}"))
        else:
            producers[key] = package

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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("audit", "check"))
    parser.add_argument("roots", nargs="+", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args(argv)

    manifests = discover_manifests(args.roots)
    packages: list[Package] = []
    findings: list[Finding] = []
    for manifest in manifests:
        package, package_findings = load_package(manifest)
        findings.extend(package_findings)
        if package:
            packages.append(package)
    if not manifests:
        findings.append(Finding("NO_MANIFESTS", "", "no .zpkg.toml manifests found"))

    findings = audit(packages, findings)
    report = {
        "schema": "zed.role-contract-audit/v1",
        "packages": [asdict(package) | {"coordinate": package.coordinate} for package in packages],
        "findings": [asdict(finding) for finding in findings],
        "ok": not findings,
    }
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(encoded, encoding="utf-8")
    else:
        sys.stdout.write(encoded)
    return 0 if args.mode == "audit" or not findings else 2


if __name__ == "__main__":
    raise SystemExit(main())
