#!/usr/bin/env python3
"""Validate the CLI's Zed dependency graph and sibling package targets."""

from __future__ import annotations

import argparse
import pathlib
import sys
import tomllib
from typing import Any

ROOT = pathlib.Path(__file__).resolve().parents[1]
REQUIRED = {
    "zed-pkg/zed-interfaces",
    "zed-pkg/zed-clients-rust",
}


def load(path: pathlib.Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--interfaces-manifest", type=pathlib.Path)
    parser.add_argument("--clients-manifest", type=pathlib.Path)
    args = parser.parse_args()

    manifest = load(ROOT / ".zpkg.toml")
    lock = load(ROOT / ".zpkg.lock")
    cargo = load(ROOT / "Cargo.toml")
    dependencies = manifest.get("dependencies", {})
    errors: list[str] = []

    if not isinstance(dependencies, dict):
        errors.append(".zpkg.toml [dependencies] must be a table")
        dependencies = {}

    missing = sorted(REQUIRED - set(dependencies))
    errors.extend(f"missing Zed dependency: {name}" for name in missing)

    for name in dependencies:
        package = name.lower().split("/", 1)[-1]
        if package.endswith("-infra"):
            errors.append(f"CLI must not import infrastructure package: {name}")

    package = manifest.get("package", {})
    repository = package.get("repository", {}) if isinstance(package, dict) else {}
    if repository.get("url") != "https://github.com/zed-pkg/zed-cli":
        errors.append("package.repository.url does not point at zed-pkg/zed-cli")

    if lock.get("version") != 1:
        errors.append(".zpkg.lock must use version = 1")

    cargo_dependencies = cargo.get("dependencies", {})
    if not isinstance(cargo_dependencies, dict) or "zed-interfaces" not in cargo_dependencies:
        errors.append("Cargo.toml must retain the native zed-interfaces dependency")

    if args.interfaces_manifest:
        interfaces = load(args.interfaces_manifest)
        table = interfaces.get("package", {})
        if not isinstance(table, dict) or table.get("name") != "zed-interfaces":
            errors.append("sibling interfaces manifest does not provide zed-interfaces")

    if args.clients_manifest:
        clients = load(args.clients_manifest)
        targets = clients.get("targets", {})
        if not isinstance(targets, dict) or "rust" not in targets:
            errors.append("sibling clients manifest does not provide the rust target")

    if errors:
        for error in errors:
            print(f"error: {error}", file=sys.stderr)
        return 1

    print("validated zed-cli -> interfaces + clients-rust package edges")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
