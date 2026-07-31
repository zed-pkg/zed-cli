#!/usr/bin/env python3
"""Assertions for Zed's self-contained copy-install ownership contract."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
from pathlib import Path
from typing import Iterable, NoReturn


def fail(message: str) -> NoReturn:
    raise AssertionError(message)


def exactly_one(paths: Iterable[Path], description: str) -> Path:
    matches = sorted(paths)
    if len(matches) != 1:
        fail(f"expected exactly one {description}, found {len(matches)}: {matches}")
    return matches[0]


def assert_regular_tree(root: Path) -> list[Path]:
    if not root.is_dir():
        fail(f"missing copied directory: {root}")
    files: list[Path] = []
    for current, directories, names in os.walk(root, followlinks=False):
        current_path = Path(current)
        for name in directories:
            candidate = current_path / name
            if candidate.is_symlink():
                fail(f"copy tree contains a directory symlink: {candidate}")
        for name in names:
            candidate = current_path / name
            if candidate.is_symlink():
                fail(f"copy tree contains a file symlink: {candidate}")
            if not candidate.is_file():
                fail(f"copy tree contains a non-regular file: {candidate}")
            files.append(candidate)
    return sorted(files)


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def assert_same_bytes(left: Path, right: Path) -> None:
    if left.read_bytes() != right.read_bytes():
        fail(f"file contents differ: {left} != {right}")


def assert_same_mode(left: Path, right: Path) -> None:
    left_mode = stat.S_IMODE(left.stat().st_mode)
    right_mode = stat.S_IMODE(right.stat().st_mode)
    if left_mode != right_mode:
        fail(f"file modes differ: {left}={oct(left_mode)} != {right}={oct(right_mode)}")


def assert_distinct_inode(source: Path, destination: Path) -> None:
    if os.name == "nt":
        return
    source_stat = source.stat()
    destination_stat = destination.stat()
    if (source_stat.st_dev, source_stat.st_ino) == (
        destination_stat.st_dev,
        destination_stat.st_ino,
    ):
        fail(f"copy shares the source device+inode: {source} -> {destination}")


def assert_executable(path: Path) -> None:
    mode = path.stat().st_mode
    if mode & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH) == 0:
        fail(f"expected executable permission bits on {path}: {oct(mode)}")


def project_roots(project: Path) -> list[Path]:
    return [
        project / ".vendor/.zed",
        project / "node_modules/@zed-pkg",
        project / ".zed",
    ]


def snapshot(project: Path) -> dict[str, object]:
    records: list[dict[str, object]] = []
    for root in project_roots(project):
        for path in assert_regular_tree(root):
            relative = path.relative_to(project).as_posix()
            records.append(
                {
                    "path": relative,
                    "mode": stat.S_IMODE(path.stat().st_mode),
                    "sha256": file_digest(path),
                }
            )
    lockfile = project / ".zpkg.lock"
    if not lockfile.is_file() or lockfile.is_symlink():
        fail(f"missing regular lockfile: {lockfile}")
    records.append(
        {
            "path": ".zpkg.lock",
            "mode": stat.S_IMODE(lockfile.stat().st_mode),
            "sha256": file_digest(lockfile),
        }
    )
    records.sort(key=lambda record: str(record["path"]))
    serialized = json.dumps(records, sort_keys=True, separators=(",", ":")).encode()
    return {
        "sha256": hashlib.sha256(serialized).hexdigest(),
        "files": records,
    }


def assert_copy_contract(project: Path, zed_home: Path) -> dict[str, str]:
    package = project / ".vendor/.zed/zed-pkg/docker-node-lib"
    adapter = project / "node_modules/@zed-pkg/docker-node-lib"
    hoisted = project / ".vendor/.zed/.bin/docker-node-tool"

    for root in project_roots(project):
        assert_regular_tree(root)

    store_source = exactly_one(
        zed_home.glob("store/v1/*/*/pkg/src/index.js"),
        "content-addressed source file",
    )
    store_bin = exactly_one(
        zed_home.glob("store/v1/*/*/pkg/bin/docker-node-tool"),
        "content-addressed bin file",
    )
    build_output = exactly_one(
        zed_home.glob("builds/v1/*/*/*/pkg/generated/output.txt"),
        "build-cache output",
    )

    package_source = package / "src/index.js"
    adapter_source = adapter / "src/index.js"
    package_bin = package / "bin/docker-node-tool"
    adapter_bin = adapter / "bin/docker-node-tool"
    package_output = package / "generated/output.txt"
    adapter_output = adapter / "generated/output.txt"

    for candidate in [
        package_source,
        adapter_source,
        package_bin,
        adapter_bin,
        hoisted,
        package_output,
        adapter_output,
    ]:
        if not candidate.is_file() or candidate.is_symlink():
            fail(f"expected an independently materialized regular file: {candidate}")

    assert_same_bytes(store_source, package_source)
    assert_same_bytes(store_source, adapter_source)
    assert_same_bytes(store_bin, package_bin)
    assert_same_bytes(store_bin, adapter_bin)
    assert_same_bytes(build_output, package_output)
    assert_same_bytes(build_output, adapter_output)
    assert_same_mode(store_bin, package_bin)
    assert_same_mode(store_bin, adapter_bin)

    for source, destination in [
        (store_source, package_source),
        (store_source, adapter_source),
        (store_bin, package_bin),
        (store_bin, adapter_bin),
        (package_bin, hoisted),
        (build_output, package_output),
        (build_output, adapter_output),
    ]:
        assert_distinct_inode(source, destination)

    assert_executable(hoisted)

    immutable_store_source = store_source.read_bytes()
    independent_adapter_source = adapter_source.read_bytes()
    package_source.write_bytes(package_source.read_bytes() + b"\n// project-owned mutation\n")
    if store_source.read_bytes() != immutable_store_source:
        fail("mutating the project copy changed the content-addressed store")
    if adapter_source.read_bytes() != independent_adapter_source:
        fail("mutating the package tree changed the independent Node adapter copy")

    immutable_build_output = build_output.read_bytes()
    independent_adapter_output = adapter_output.read_bytes()
    package_output.write_bytes(package_output.read_bytes() + b"project-owned mutation\n")
    if build_output.read_bytes() != immutable_build_output:
        fail("mutating a copied build output changed the build cache")
    if adapter_output.read_bytes() != independent_adapter_output:
        fail("mutating a build output changed the independent adapter copy")

    independent_package_bin = package_bin.read_bytes()
    hoisted.write_bytes(hoisted.read_bytes() + b"\n# independently hoisted mutation\n")
    if package_bin.read_bytes() != independent_package_bin:
        fail("mutating the hoisted executable changed the installed package executable")
    assert_executable(hoisted)

    return {
        "store_entry": str(store_source.parents[2]),
        "build_entry": str(build_output.parents[2]),
        "project_package": str(package),
        "adapter_package": str(adapter),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    subcommands = parser.add_subparsers(dest="command", required=True)

    snapshot_parser = subcommands.add_parser("snapshot")
    snapshot_parser.add_argument("--project", type=Path, required=True)

    assert_parser = subcommands.add_parser("assert-copy")
    assert_parser.add_argument("--project", type=Path, required=True)
    assert_parser.add_argument("--zed-home", type=Path, required=True)

    args = parser.parse_args()
    project = args.project.resolve()

    if args.command == "snapshot":
        print(json.dumps(snapshot(project), indent=2, sort_keys=True))
        return

    result = assert_copy_contract(project, args.zed_home.resolve())
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
