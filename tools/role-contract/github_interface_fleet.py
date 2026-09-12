#!/usr/bin/env python3
"""Discover and audit *-interfaces families across GitHub organizations.

The scanner is intentionally read-only. It can consume a deterministic JSON
snapshot for CI/tests or use the GitHub REST API when `--org` is supplied.
A server is conformant only when it has Zed role/family metadata, an exact
interface dependency, a resolver-created lock, and a Rust source import.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import sys
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable

INTERFACE_RE = re.compile(r"^(?P<family>.+)-interfaces(?:\.rs)?$")
SERVER_SUFFIXES = {
    "web": ("-web-server.rs", "-web-server"),
    "api": ("-api-server.rs", "-api-server"),
}


@dataclass(frozen=True)
class Repository:
    org: str
    name: str
    full_name: str
    default_branch: str = "main"
    archived: bool = False
    files: dict[str, str] = field(default_factory=dict, compare=False)


@dataclass(frozen=True)
class ConsumerAudit:
    kind: str
    repository: str | None
    conformant: bool
    findings: tuple[str, ...]


@dataclass(frozen=True)
class FamilyAudit:
    org: str
    family: str
    interfaces: str
    producer_findings: tuple[str, ...]
    web: ConsumerAudit
    api: ConsumerAudit

    @property
    def conformant(self) -> bool:
        return not self.producer_findings and self.web.conformant and self.api.conformant

    def to_json(self) -> dict[str, Any]:
        return {
            "org": self.org,
            "family": self.family,
            "interfaces": self.interfaces,
            "conformant": self.conformant,
            "producer_findings": list(self.producer_findings),
            "web": {
                "repository": self.web.repository,
                "conformant": self.web.conformant,
                "findings": list(self.web.findings),
            },
            "api": {
                "repository": self.api.repository,
                "conformant": self.api.conformant,
                "findings": list(self.api.findings),
            },
        }


class GitHubReader:
    def __init__(self, token: str | None, api_url: str = "https://api.github.com") -> None:
        self.token = token
        self.api_url = api_url.rstrip("/")

    def _json(self, path: str) -> Any:
        request = urllib.request.Request(
            f"{self.api_url}{path}",
            headers={
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
                "User-Agent": "zed-role-contract-interface-fleet",
                **({"Authorization": f"Bearer {self.token}"} if self.token else {}),
            },
        )
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"GitHub API {error.code} for {path}: {detail[:300]}") from error

    def list_org(self, org: str) -> list[Repository]:
        repositories: list[Repository] = []
        page = 1
        while True:
            encoded = urllib.parse.quote(org, safe="")
            payload = self._json(
                f"/orgs/{encoded}/repos?type=all&sort=full_name&per_page=100&page={page}"
            )
            if not isinstance(payload, list):
                raise RuntimeError(f"unexpected GitHub repository response for {org}")
            for item in payload:
                if not isinstance(item, dict):
                    continue
                repositories.append(
                    Repository(
                        org=org,
                        name=str(item.get("name", "")),
                        full_name=str(item.get("full_name", "")),
                        default_branch=str(item.get("default_branch") or "main"),
                        archived=bool(item.get("archived")),
                    )
                )
            if len(payload) < 100:
                break
            page += 1
        return repositories

    def hydrate(self, repository: Repository) -> Repository:
        owner, name = repository.full_name.split("/", 1)
        branch = urllib.parse.quote(repository.default_branch, safe="")
        tree = self._json(f"/repos/{owner}/{name}/git/trees/{branch}?recursive=1")
        entries = tree.get("tree", []) if isinstance(tree, dict) else []
        paths = {
            str(entry.get("path"))
            for entry in entries
            if isinstance(entry, dict) and entry.get("type") == "blob"
        }
        wanted = {".zpkg.toml", ".zpkg.lock", "Cargo.toml"}
        wanted.update(
            path
            for path in paths
            if path.endswith(".rs")
            and (path.startswith("src/") or path.startswith("tests/"))
        )
        files: dict[str, str] = {}
        for path in sorted(wanted & paths):
            encoded_path = "/".join(urllib.parse.quote(part, safe="") for part in path.split("/"))
            payload = self._json(
                f"/repos/{owner}/{name}/contents/{encoded_path}?ref={branch}"
            )
            if not isinstance(payload, dict) or payload.get("encoding") != "base64":
                continue
            raw = base64.b64decode(str(payload.get("content", "")), validate=False)
            files[path] = raw.decode("utf-8", errors="replace")
        return Repository(
            org=repository.org,
            name=repository.name,
            full_name=repository.full_name,
            default_branch=repository.default_branch,
            archived=repository.archived,
            files=files,
        )


def parse_manifest(repository: Repository) -> tuple[dict[str, Any] | None, str | None]:
    value = repository.files.get(".zpkg.toml")
    if value is None:
        return None, "missing .zpkg.toml"
    try:
        return tomllib.loads(value), None
    except tomllib.TOMLDecodeError as error:
        return None, f"invalid .zpkg.toml: {error}"


def normalize_crate(name: str) -> str:
    return name.replace("-", "_").removesuffix(".rs")


def producer_findings(repository: Repository, family: str) -> tuple[str, ...]:
    manifest, error = parse_manifest(repository)
    if error:
        return (error,)
    assert manifest is not None
    package = manifest.get("package")
    findings: list[str] = []
    if not isinstance(package, dict):
        findings.append("missing [package]")
    else:
        if package.get("role") != "interfaces":
            findings.append("package.role must be interfaces")
        if package.get("family") != family:
            findings.append(f"package.family must be {family}")
    dependencies = manifest.get("dependencies")
    if isinstance(dependencies, dict) and dependencies:
        findings.append("interfaces producer must not depend on family producers")
    return tuple(findings)


def consumer_findings(
    repository: Repository,
    kind: str,
    family: str,
    interface_coordinate: str,
    interface_repository_name: str,
) -> tuple[str, ...]:
    findings: list[str] = []
    manifest, error = parse_manifest(repository)
    if error:
        findings.append(error)
        return tuple(findings)
    assert manifest is not None
    package = manifest.get("package")
    if not isinstance(package, dict):
        findings.append("missing [package]")
    else:
        if package.get("role") != "server":
            findings.append("package.role must be server")
        if package.get("family") != family:
            findings.append(f"package.family must be {family}")
    dependencies = manifest.get("dependencies")
    if not isinstance(dependencies, dict) or interface_coordinate not in dependencies:
        findings.append(f"missing Zed dependency {interface_coordinate}")
    if ".zpkg.lock" not in repository.files:
        findings.append("missing resolver-created .zpkg.lock")

    rust_sources = "\n".join(
        value
        for path, value in repository.files.items()
        if path.endswith(".rs") and (path.startswith("src/") or path.startswith("tests/"))
    )
    crate = normalize_crate(interface_repository_name)
    import_patterns = (
        rf"\buse\s+{re.escape(crate)}\b",
        rf"\bextern\s+crate\s+{re.escape(crate)}\b",
        rf"\b{re.escape(crate)}::",
    )
    if not rust_sources:
        findings.append(f"no Rust sources were available for {kind} server audit")
    elif not any(re.search(pattern, rust_sources) for pattern in import_patterns):
        findings.append(f"no source import of generated crate {crate}")
    return tuple(findings)


def choose_consumer(
    repositories: dict[str, Repository], family: str, kind: str
) -> Repository | None:
    for suffix in SERVER_SUFFIXES[kind]:
        candidate = repositories.get(f"{family}{suffix}")
        if candidate is not None and not candidate.archived:
            return candidate
    return None


def audit(repositories: Iterable[Repository]) -> list[FamilyAudit]:
    by_org: dict[str, dict[str, Repository]] = {}
    for repository in repositories:
        by_org.setdefault(repository.org, {})[repository.name] = repository

    audits: list[FamilyAudit] = []
    for org, org_repositories in sorted(by_org.items()):
        for interface_repository in sorted(org_repositories.values(), key=lambda item: item.name):
            match = INTERFACE_RE.fullmatch(interface_repository.name)
            if match is None or interface_repository.archived:
                continue
            family = match.group("family")
            coordinate = interface_repository.full_name
            consumers: dict[str, ConsumerAudit] = {}
            for kind in ("web", "api"):
                consumer = choose_consumer(org_repositories, family, kind)
                if consumer is None:
                    consumers[kind] = ConsumerAudit(
                        kind=kind,
                        repository=None,
                        conformant=False,
                        findings=(f"missing {kind} server repository",),
                    )
                    continue
                findings = consumer_findings(
                    consumer,
                    kind,
                    family,
                    coordinate,
                    interface_repository.name,
                )
                consumers[kind] = ConsumerAudit(
                    kind=kind,
                    repository=consumer.full_name,
                    conformant=not findings,
                    findings=findings,
                )
            audits.append(
                FamilyAudit(
                    org=org,
                    family=family,
                    interfaces=coordinate,
                    producer_findings=producer_findings(interface_repository, family),
                    web=consumers["web"],
                    api=consumers["api"],
                )
            )
    return audits


def load_snapshot(path: Path) -> list[Repository]:
    payload = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(payload, list):
        raise SystemExit("snapshot must be a JSON array")
    repositories: list[Repository] = []
    for item in payload:
        if not isinstance(item, dict):
            raise SystemExit("snapshot repositories must be objects")
        full_name = str(item["full_name"])
        org, _, name = full_name.partition("/")
        files = item.get("files", {})
        if not isinstance(files, dict) or not all(
            isinstance(key, str) and isinstance(value, str)
            for key, value in files.items()
        ):
            raise SystemExit(f"{full_name}: files must map strings to strings")
        repositories.append(
            Repository(
                org=str(item.get("org") or org),
                name=str(item.get("name") or name),
                full_name=full_name,
                default_branch=str(item.get("default_branch") or "main"),
                archived=bool(item.get("archived")),
                files=dict(files),
            )
        )
    return repositories


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--snapshot", type=Path)
    parser.add_argument("--org", action="append", default=[])
    parser.add_argument("--deep", action="store_true")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--strict", action="store_true")
    args = parser.parse_args()

    if bool(args.snapshot) == bool(args.org):
        parser.error("provide either --snapshot or one or more --org values")

    if args.snapshot:
        repositories = load_snapshot(args.snapshot)
    else:
        reader = GitHubReader(os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN"))
        repositories = [repository for org in args.org for repository in reader.list_org(org)]
        if args.deep:
            relevant_names: set[str] = set()
            for repository in repositories:
                match = INTERFACE_RE.fullmatch(repository.name)
                if match:
                    family = match.group("family")
                    relevant_names.add(repository.name)
                    for suffixes in SERVER_SUFFIXES.values():
                        relevant_names.update(f"{family}{suffix}" for suffix in suffixes)
            repositories = [
                reader.hydrate(repository) if repository.name in relevant_names else repository
                for repository in repositories
            ]

    audits = audit(repositories)
    result = {
        "schema_version": 1,
        "family_count": len(audits),
        "conformant_count": sum(item.conformant for item in audits),
        "families": [item.to_json() for item in audits],
    }
    rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
    else:
        sys.stdout.write(rendered)
    if args.strict and any(not item.conformant for item in audits):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
