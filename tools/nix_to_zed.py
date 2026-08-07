#!/usr/bin/env python3
"""Seal one closure-free realized Nix output as a normal Zed package.

This is the executable DEN-1419 canary for the canonical
``zed.nix-adapter/v1`` contract in ``zed-interfaces``. Nix remains the sole
resolution authority. The command accepts one explicit already-realized
output, verifies strict portability, creates deterministic package bytes, and
writes an external canonical adapter record plus a hash-binding bridge.

The sealed archive never embeds the exact ``/nix/store`` path and never needs
Nix at install or runtime.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import sys
import tarfile
import tempfile
import tomllib
from typing import Any, Iterable, Mapping, Sequence
from urllib.parse import urlparse

ADAPTER_SCHEMA = "zed.nix-adapter/v1"
RUNTIME_SCHEMA = "zed.nix-runtime-provenance/v1"
BRIDGE_SCHEMA = "zed.nix-seal-bridge/v1"
HEX_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SRI_SHA256_RE = re.compile(r"^sha256-[A-Za-z0-9+/]{43}=$")
NIX_STORE_RE = re.compile(
    r"^/nix/store/[0-9abcdfghijklmnpqrsvwxyz]{32}-[A-Za-z0-9+._?-]+$"
)
STORE_BYTES_RE = re.compile(
    rb"/nix/store/[0-9abcdfghijklmnpqrsvwxyz]{32}-[A-Za-z0-9+._?-]+"
)
SLUG_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$")
SYSTEM_RE = re.compile(r"^[a-z0-9_]+-[a-z0-9_-]+$")
IDENTIFIER_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_'-]*$")
PROGRAM_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+-]*$")
REVISION_RE = re.compile(r"^(?:[0-9a-f]{40}|[0-9a-f]{64})$")
RESERVED_ARCHIVE_PATHS = {
    ".zpkg.toml",
    "README.zed-nix.md",
    "zed-nix-runtime.json",
}


class SealError(ValueError):
    """Expected validation failure."""


def fail(message: str) -> None:
    raise SealError(message)


def canonical_json_bytes(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def pretty_json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def read_json(path: Path) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise SealError(f"missing JSON input: {path}") from error
    except json.JSONDecodeError as error:
        raise SealError(f"invalid JSON in {path}: {error}") from error


def validate_hex(value: object, label: str) -> str:
    if not isinstance(value, str) or not HEX_SHA256_RE.fullmatch(value):
        fail(f"{label} must be 64 lowercase hexadecimal SHA-256 characters")
    return value


def validate_sri(value: object, label: str) -> str:
    if not isinstance(value, str) or not SRI_SHA256_RE.fullmatch(value):
        fail(f"{label} must be a canonical SHA-256 SRI hash")
    return value


def validate_slug(value: str, label: str) -> str:
    if not SLUG_RE.fullmatch(value):
        fail(f"{label} must be a lowercase Zed slug, found {value!r}")
    return value


def validate_version(value: str) -> str:
    if (
        not value
        or value.strip() != value
        or any(character.isspace() for character in value)
    ):
        fail("package version must be non-empty and contain no whitespace")
    return value


def validate_system(value: str) -> str:
    if not SYSTEM_RE.fullmatch(value):
        fail(f"invalid Nix system {value!r}")
    return value


def validate_identifier(value: str, label: str) -> str:
    if not IDENTIFIER_RE.fullmatch(value):
        fail(f"{label} must be a Nix identifier, found {value!r}")
    return value


def validate_attribute(value: str) -> str:
    if not value or not all(IDENTIFIER_RE.fullmatch(part) for part in value.split(".")):
        fail(f"invalid standard Nix flake attribute path {value!r}")
    return value


def validate_revision(value: str, label: str) -> str:
    value = value.lower()
    if not REVISION_RE.fullmatch(value):
        fail(f"{label} must be an immutable 40- or 64-character hexadecimal revision")
    return value


def validate_locked_ref(value: str) -> str:
    if (
        not value
        or value.strip() != value
        or any(character.isspace() for character in value)
    ):
        fail("locked ref must be non-empty and contain no whitespace")
    if value.startswith("/nix/store/") or value.startswith("path:/nix/store/"):
        return value
    if "narHash=sha256-" in value:
        return value
    revision_tokens = re.findall(r"[0-9A-Fa-f]{40}|[0-9A-Fa-f]{64}", value)
    if revision_tokens:
        return value
    fail("locked ref must contain immutable revision or NAR-hash evidence")


def safe_relative(value: str, label: str) -> str:
    if not value or "\0" in value or "\\" in value:
        fail(f"{label} must be a portable relative path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        fail(f"{label} escapes the package: {value!r}")
    return value


def parse_package(value: str) -> tuple[str, str, str]:
    if "@" not in value or "/" not in value:
        fail("--as-package must be org/name@version")
    package, version = value.rsplit("@", 1)
    org, name = package.split("/", 1)
    return (
        validate_slug(org, "package org"),
        validate_slug(name, "package name"),
        validate_version(version),
    )


def parse_bins(values: Sequence[str]) -> dict[str, str]:
    result: dict[str, str] = {}
    for value in values:
        if "=" not in value:
            fail(f"invalid --bin {value!r}; expected NAME=relative/path")
        name, path = value.split("=", 1)
        if not PROGRAM_RE.fullmatch(name):
            fail(f"invalid Zed binary name {name!r}")
        if name in result:
            fail(f"duplicate Zed binary name {name!r}")
        result[name] = safe_relative(path, f"binary {name}")
    return dict(sorted(result.items()))


def parse_path_info(value: object, store_path: str) -> Mapping[str, Any]:
    candidates: list[Mapping[str, Any]] = []
    if isinstance(value, list):
        candidates = [item for item in value if isinstance(item, Mapping)]
    elif isinstance(value, Mapping):
        selected = value.get(store_path)
        if isinstance(selected, Mapping):
            record = dict(selected)
            record.setdefault("path", store_path)
            return record
        candidates = [value]
    for candidate in candidates:
        if candidate.get("path") == store_path:
            return candidate
    fail("path-info JSON has no record for the selected output")


def validate_store_path(path: Path, allow_local: bool) -> str:
    value = str(path)
    if not allow_local and not NIX_STORE_RE.fullmatch(value):
        fail(
            "strict sealing requires a /nix/store output; "
            "fixtures require --allow-local-store"
        )
    if path.is_symlink() or not path.is_dir():
        fail("selected realized output must be a real directory, not a symlink")
    return value


def validate_tree(root: Path, bins: Mapping[str, str]) -> list[str]:
    for reserved in RESERVED_ARCHIVE_PATHS:
        if (root / reserved).exists() or (root / reserved).is_symlink():
            fail(f"realized output collides with reserved package path {reserved}")

    paths: list[str] = []
    root_real = root.resolve()
    for current, directories, files in os.walk(
        root, topdown=True, followlinks=False
    ):
        current_path = Path(current)
        directories.sort()
        files.sort()
        for name in list(directories) + list(files):
            path = current_path / name
            relative = path.relative_to(root).as_posix()
            paths.append(relative)
            metadata = os.lstat(path)
            if stat.S_ISDIR(metadata.st_mode):
                continue
            if stat.S_ISLNK(metadata.st_mode):
                target = os.readlink(path)
                if os.path.isabs(target) or "/nix/store/" in target:
                    fail(f"unsafe symlink in realized output: {relative} -> {target}")
                try:
                    (path.parent / target).resolve().relative_to(root_real)
                except ValueError:
                    fail(f"symlink escapes realized output: {relative} -> {target}")
                if name in directories:
                    directories.remove(name)
                continue
            if not stat.S_ISREG(metadata.st_mode):
                fail(f"unsupported file type in realized output: {relative}")
            with path.open("rb") as stream:
                tail = b""
                for block in iter(lambda: stream.read(1024 * 1024), b""):
                    sample = tail + block
                    match = STORE_BYTES_RE.search(sample)
                    if match:
                        fail(
                            f"runtime store reference in {relative}: "
                            f"{match.group(0).decode('ascii', errors='replace')}"
                        )
                    tail = sample[-256:]

    for name, relative in bins.items():
        path = root / relative
        try:
            resolved = path.resolve(strict=True)
            resolved.relative_to(root_real)
        except (FileNotFoundError, ValueError):
            fail(f"declared binary {name} does not resolve inside the realized output")
        if (
            not resolved.is_file()
            or stat.S_IMODE(resolved.stat().st_mode) & 0o111 == 0
        ):
            fail(f"declared binary {name} is missing or not executable: {relative}")
    return sorted(paths)


def validate_repository(value: str) -> str:
    parsed = urlparse(value)
    if parsed.scheme != "https" or not parsed.netloc:
        fail("--repository must be an HTTPS source URL")
    return value


def preflight(paths: Iterable[Path], force: bool) -> None:
    for path in paths:
        if path.is_symlink():
            fail(f"refusing symlink output: {path}")
        if path.exists() and not force:
            fail(f"refusing to overwrite existing output without --force: {path}")


def atomic_write(path: Path, content: bytes, mode: int, force: bool) -> None:
    preflight([path], force)
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(temporary, mode)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def tar_info(name: str, mode: int, size: int = 0) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = 0
    info.mode = mode
    info.size = size
    return info


def normalized_mode(mode: int, directory: bool = False) -> int:
    if directory:
        return 0o755
    return 0o755 if mode & 0o111 else 0o644


def add_tree(archive: tarfile.TarFile, root: Path) -> None:
    paths: list[Path] = []
    for current, directories, files in os.walk(
        root, topdown=True, followlinks=False
    ):
        current_path = Path(current)
        directories.sort()
        files.sort()
        for name in list(directories) + list(files):
            path = current_path / name
            paths.append(path)
            if path.is_symlink() and name in directories:
                directories.remove(name)
    for path in sorted(
        paths, key=lambda candidate: candidate.relative_to(root).as_posix()
    ):
        relative = path.relative_to(root).as_posix()
        metadata = os.lstat(path)
        if stat.S_ISDIR(metadata.st_mode):
            info = tar_info(
                relative + "/", normalized_mode(metadata.st_mode, True)
            )
            info.type = tarfile.DIRTYPE
            archive.addfile(info)
        elif stat.S_ISLNK(metadata.st_mode):
            info = tar_info(relative, 0o777)
            info.type = tarfile.SYMTYPE
            info.linkname = os.readlink(path)
            archive.addfile(info)
        elif stat.S_ISREG(metadata.st_mode):
            info = tar_info(
                relative, normalized_mode(metadata.st_mode), metadata.st_size
            )
            with path.open("rb") as stream:
                archive.addfile(info, stream)
        else:
            fail(f"unsupported file type while sealing {relative}")


def build_archive(
    root: Path, extras: Mapping[str, tuple[bytes, int]], destination: Path
) -> None:
    with destination.open("wb") as raw:
        with gzip.GzipFile(
            filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0
        ) as compressed:
            with tarfile.open(
                fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT
            ) as archive:
                add_tree(archive, root)
                for relative, (content, mode) in sorted(extras.items()):
                    info = tar_info(relative, mode, len(content))
                    archive.addfile(info, io.BytesIO(content))


def toml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def render_manifest(
    org: str,
    name: str,
    version: str,
    description: str,
    license_name: str,
    repository: str,
    bins: Mapping[str, str],
) -> bytes:
    lines = [
        "[package]",
        f"org = {toml_string(org)}",
        f"name = {toml_string(name)}",
        f"version = {toml_string(version)}",
        f"description = {toml_string(description)}",
        f"license = {toml_string(license_name)}",
        'keywords = ["nix", "sealed", "immutable"]',
        "",
        "[package.repository]",
        'vcs = "git"',
        f"url = {toml_string(repository)}",
    ]
    if bins:
        lines.extend(["", "[bin]"])
        for binary, path in bins.items():
            lines.append(f"{toml_string(binary)} = {toml_string(path)}")
    lines.append("")
    return "\n".join(lines).encode("utf-8")


def strict_policy() -> dict[str, object]:
    return {
        "profile": "strict-v1",
        "pure_evaluation": True,
        "import_from_derivation": False,
        "sandbox_required": True,
        "builder_network": "disabled",
        "dirty_source": False,
        "publishable": True,
    }


def canonical_adapter(
    *,
    package: Mapping[str, object],
    locked_ref: str,
    flake_lock_sha256: str,
    attribute: str,
    system: str,
    output_name: str,
    derivation_sha256: str,
    store_path: str,
    nar_hash: str,
    nar_size: int,
    signatures: Sequence[str],
    nix_version: str,
    store_info_json_version: int,
    artifact_sha256: str,
    artifact_size: int,
) -> dict[str, object]:
    return {
        "direction": "nix-to-zed",
        "schema": ADAPTER_SCHEMA,
        "package": dict(package),
        "source": {
            "locked_ref": locked_ref,
            "flake_lock_sha256": flake_lock_sha256,
            "attribute": attribute,
            "realized": {
                "system": system,
                "output": output_name,
                "derivation_json_sha256": derivation_sha256,
                "store_path": store_path,
                "nar_hash": nar_hash,
                "nar_size": nar_size,
                "references": [],
                "signatures": sorted(signatures),
                "nix_version": nix_version,
                "store_info_json_version": store_info_json_version,
            },
        },
        "artifact": {
            "format": "tar.gz",
            "sha256": artifact_sha256,
            "size": artifact_size,
        },
        "policy": strict_policy(),
    }


def runtime_projection(
    *,
    package: Mapping[str, object],
    locked_ref: str,
    flake_lock_sha256: str,
    attribute: str,
    system: str,
    output_name: str,
    derivation_sha256: str,
    store_path_sha256: str,
    nar_hash: str,
    nar_size: int,
    nix_version: str,
    store_info_json_version: int,
    repository: str,
    source_revision: str,
    sealed_paths: Sequence[str],
) -> dict[str, object]:
    return {
        "schema": RUNTIME_SCHEMA,
        "package": dict(package),
        "origin": {
            "locked_ref": locked_ref,
            "flake_lock_sha256": flake_lock_sha256,
            "attribute": attribute,
            "system": system,
            "output": output_name,
            "derivation_json_sha256": derivation_sha256,
            "store_path_sha256": store_path_sha256,
            "nar_hash": nar_hash,
            "nar_size": nar_size,
            "nix_version": nix_version,
            "store_info_json_version": store_info_json_version,
            "repository": repository,
            "source_revision": source_revision,
        },
        "policy": {
            **strict_policy(),
            "nix_required_at_zed_runtime": False,
            "portable_reference_count": 0,
        },
        "sealed_paths": list(sealed_paths),
    }


def safe_tar_members(archive: tarfile.TarFile) -> list[tarfile.TarInfo]:
    members = archive.getmembers()
    names: set[str] = set()
    for member in members:
        name = member.name.rstrip("/")
        safe_relative(name, "archive member")
        if name in names:
            fail(f"duplicate archive member: {name}")
        names.add(name)
        if not (member.isfile() or member.isdir() or member.issym()):
            fail(f"unsupported archive member type: {name}")
        if member.issym():
            target = member.linkname
            if os.path.isabs(target) or "/nix/store/" in target:
                fail(f"unsafe archive symlink: {name} -> {target}")
            resolved = PurePosixPath(name).parent.joinpath(target)
            if any(part == ".." for part in resolved.parts):
                fail(f"archive symlink escapes package: {name} -> {target}")
    return members


def validate_policy(value: object) -> Mapping[str, object]:
    expected = strict_policy()
    if not isinstance(value, Mapping) or dict(value) != expected:
        fail(
            "canonical adapter does not contain strict-v1 publishable policy evidence"
        )
    return value


def validate_adapter_shape(
    adapter: object, *, allow_local_store: bool = False
) -> Mapping[str, Any]:
    if not isinstance(adapter, Mapping):
        fail("canonical adapter must be a JSON object")
    if (
        adapter.get("direction") != "nix-to-zed"
        or adapter.get("schema") != ADAPTER_SCHEMA
    ):
        fail("canonical adapter has unsupported direction or schema")
    package = adapter.get("package")
    if not isinstance(package, Mapping):
        fail("canonical adapter lacks package identity")
    validate_slug(str(package.get("org", "")), "adapter package org")
    validate_slug(str(package.get("name", "")), "adapter package name")
    validate_version(str(package.get("version", "")))
    source = adapter.get("source")
    if not isinstance(source, Mapping):
        fail("canonical adapter lacks Nix source evidence")
    validate_locked_ref(str(source.get("locked_ref", "")))
    validate_hex(source.get("flake_lock_sha256"), "adapter flake.lock")
    validate_attribute(str(source.get("attribute", "")))
    realized = source.get("realized")
    if not isinstance(realized, Mapping):
        fail("canonical adapter lacks realized output evidence")
    validate_system(str(realized.get("system", "")))
    validate_identifier(str(realized.get("output", "")), "adapter output")
    validate_hex(realized.get("derivation_json_sha256"), "adapter derivation JSON")
    adapter_store_path = str(realized.get("store_path", ""))
    if not NIX_STORE_RE.fullmatch(adapter_store_path) and not (
        allow_local_store and Path(adapter_store_path).is_absolute()
    ):
        fail("canonical adapter has invalid Nix store path")
    validate_sri(realized.get("nar_hash"), "adapter NAR hash")
    if not isinstance(realized.get("nar_size"), int) or realized["nar_size"] <= 0:
        fail("canonical adapter NAR size must be greater than zero")
    if realized.get("references", []) != []:
        fail("contract v1 Nix-to-Zed adapter must be closure-free")
    signatures = realized.get("signatures", [])
    if not isinstance(signatures, list) or any(
        not isinstance(item, str)
        or not item
        or any(character.isspace() for character in item)
        for item in signatures
    ):
        fail("canonical adapter signatures must be non-empty tokens")
    if (
        not isinstance(realized.get("nix_version"), str)
        or not realized["nix_version"].strip()
    ):
        fail("canonical adapter must record Nix version")
    if realized.get("store_info_json_version") not in (1, 2, 3):
        fail("canonical adapter store-info JSON version is unsupported")
    artifact = adapter.get("artifact")
    if not isinstance(artifact, Mapping) or artifact.get("format") != "tar.gz":
        fail("canonical adapter artifact must be tar.gz")
    validate_hex(artifact.get("sha256"), "adapter artifact")
    if not isinstance(artifact.get("size"), int) or artifact["size"] <= 0:
        fail("canonical adapter artifact size must be greater than zero")
    validate_policy(adapter.get("policy"))
    return adapter


def seal(args: argparse.Namespace) -> dict[str, object]:
    org, name, version = parse_package(args.as_package)
    bins = parse_bins(args.bin)
    package: dict[str, object] = {"org": org, "name": name, "version": version}
    if args.target is not None:
        package["target"] = validate_slug(args.target, "package target")
    locked_ref = validate_locked_ref(args.locked_ref)
    attribute = validate_attribute(args.attribute)
    system = validate_system(args.system)
    output_name = validate_identifier(args.output, "selected output")
    repository = validate_repository(args.repository)
    source_revision = validate_revision(args.source_revision, "source revision")
    if source_revision.lower() not in locked_ref.lower():
        fail("source revision is not present in the immutable locked ref")

    store_path = args.store_path.resolve()
    store_path_value = validate_store_path(store_path, args.allow_local_store)
    sealed_paths = validate_tree(store_path, bins)

    path_info = parse_path_info(read_json(args.path_info), store_path_value)
    nar_hash = validate_sri(path_info.get("narHash"), "path-info NAR hash")
    nar_size = path_info.get("narSize")
    if not isinstance(nar_size, int) or nar_size <= 0:
        fail("path-info NAR size must be greater than zero")
    references = path_info.get("references", [])
    if not isinstance(references, list) or any(
        not isinstance(item, str) for item in references
    ):
        fail("path-info references must be a string array")
    external_references = sorted(
        reference for reference in references if reference != store_path_value
    )
    if external_references:
        fail(
            "portable Nix output retains runtime references: "
            + ", ".join(external_references)
        )
    signatures = path_info.get("signatures", path_info.get("sigs", []))
    if not isinstance(signatures, list) or any(
        not isinstance(item, str) for item in signatures
    ):
        fail("path-info signatures must be a string array")
    store_info_json_version = args.store_info_json_version
    if store_info_json_version not in (1, 2, 3):
        fail("store-info JSON version must be 1, 2, or 3")

    flake_lock_bytes = args.flake_lock.read_bytes()
    read_json(args.flake_lock)
    derivation_bytes = args.derivation_json.read_bytes()
    read_json(args.derivation_json)
    flake_lock_sha256 = sha256_bytes(flake_lock_bytes)
    derivation_sha256 = sha256_bytes(derivation_bytes)
    store_path_sha256 = sha256_bytes(store_path_value.encode("utf-8"))

    manifest_bytes = render_manifest(
        org,
        name,
        version,
        args.description,
        args.license,
        repository,
        bins,
    )
    projection = runtime_projection(
        package=package,
        locked_ref=locked_ref,
        flake_lock_sha256=flake_lock_sha256,
        attribute=attribute,
        system=system,
        output_name=output_name,
        derivation_sha256=derivation_sha256,
        store_path_sha256=store_path_sha256,
        nar_hash=nar_hash,
        nar_size=nar_size,
        nix_version=args.nix_version,
        store_info_json_version=store_info_json_version,
        repository=repository,
        source_revision=source_revision,
        sealed_paths=sealed_paths,
    )
    projection_bytes = pretty_json_bytes(projection)
    readme_bytes = (
        f"# `{org}/{name}@{version}`\n\n"
        "This package was deterministically sealed from one explicitly selected, "
        "closure-free Nix output. It does not require Nix at install or runtime. "
        "The exact canonical adapter record remains beside the archive.\n"
    ).encode("utf-8")
    extras = {
        ".zpkg.toml": (manifest_bytes, 0o644),
        "README.zed-nix.md": (readme_bytes, 0o644),
        "zed-nix-runtime.json": (projection_bytes, 0o644),
    }

    out_dir = args.out_dir.resolve()
    artifact_name = f"{name}-{version}.tar.gz"
    artifact_path = out_dir / artifact_name
    adapter_path = out_dir / "zed-nix-adapter.json"
    bridge_path = out_dir / "bridge.json"
    preflight([artifact_path, adapter_path, bridge_path], args.force)
    out_dir.mkdir(parents=True, exist_ok=True)

    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{artifact_name}.", dir=out_dir
    )
    os.close(descriptor)
    temporary = Path(temporary_name)
    try:
        build_archive(store_path, extras, temporary)
        artifact_sha256 = sha256_file(temporary)
        artifact_size = temporary.stat().st_size
        adapter = canonical_adapter(
            package=package,
            locked_ref=locked_ref,
            flake_lock_sha256=flake_lock_sha256,
            attribute=attribute,
            system=system,
            output_name=output_name,
            derivation_sha256=derivation_sha256,
            store_path=store_path_value,
            nar_hash=nar_hash,
            nar_size=nar_size,
            signatures=signatures,
            nix_version=args.nix_version,
            store_info_json_version=store_info_json_version,
            artifact_sha256=artifact_sha256,
            artifact_size=artifact_size,
        )
        validate_adapter_shape(
            adapter, allow_local_store=args.allow_local_store
        )
        adapter_bytes = canonical_json_bytes(adapter)
        bridge = {
            "schema": BRIDGE_SCHEMA,
            "artifact_file": artifact_name,
            "artifact_sha256": artifact_sha256,
            "artifact_size": artifact_size,
            "adapter_file": "zed-nix-adapter.json",
            "adapter_sha256": sha256_bytes(adapter_bytes),
            "manifest_sha256": sha256_bytes(manifest_bytes),
            "runtime_projection_sha256": sha256_bytes(projection_bytes),
        }
        os.chmod(temporary, 0o644)
        os.replace(temporary, artifact_path)
        atomic_write(adapter_path, adapter_bytes, 0o644, args.force)
        atomic_write(bridge_path, pretty_json_bytes(bridge), 0o644, args.force)
    finally:
        temporary.unlink(missing_ok=True)
    return bridge


def verify(args: argparse.Namespace) -> dict[str, object]:
    root = args.directory.resolve()
    bridge_value = read_json(root / "bridge.json")
    if (
        not isinstance(bridge_value, Mapping)
        or bridge_value.get("schema") != BRIDGE_SCHEMA
    ):
        fail("unsupported or missing bridge schema")
    artifact_name = safe_relative(
        str(bridge_value.get("artifact_file", "")), "artifact file"
    )
    adapter_name = safe_relative(
        str(bridge_value.get("adapter_file", "")), "adapter file"
    )
    artifact_path = root / artifact_name
    adapter_path = root / adapter_name
    if artifact_path.is_symlink() or not artifact_path.is_file():
        fail("sealed artifact is missing or a symlink")
    if adapter_path.is_symlink() or not adapter_path.is_file():
        fail("canonical adapter is missing or a symlink")
    artifact_sha256 = validate_hex(
        bridge_value.get("artifact_sha256"), "bridge artifact"
    )
    if sha256_file(artifact_path) != artifact_sha256:
        fail("sealed artifact SHA-256 mismatch")
    if artifact_path.stat().st_size != bridge_value.get("artifact_size"):
        fail("sealed artifact size mismatch")
    adapter_bytes = adapter_path.read_bytes()
    if sha256_bytes(adapter_bytes) != validate_hex(
        bridge_value.get("adapter_sha256"), "bridge adapter"
    ):
        fail("canonical adapter SHA-256 mismatch")
    adapter = validate_adapter_shape(
        json.loads(adapter_bytes), allow_local_store=args.allow_local_store
    )
    if (
        adapter["artifact"]["sha256"] != artifact_sha256
        or adapter["artifact"]["size"] != artifact_path.stat().st_size
    ):
        fail("canonical adapter artifact evidence disagrees with bridge")

    manifest_bytes: bytes | None = None
    projection_bytes: bytes | None = None
    with tarfile.open(artifact_path, "r:gz") as archive:
        for member in safe_tar_members(archive):
            if not member.isfile():
                continue
            stream = archive.extractfile(member)
            if stream is None:
                fail(f"could not read archive member {member.name}")
            content = stream.read()
            match = STORE_BYTES_RE.search(content)
            if match:
                fail(
                    f"sealed artifact embeds Nix store path in {member.name}: "
                    f"{match.group(0).decode('ascii', errors='replace')}"
                )
            if member.name == ".zpkg.toml":
                manifest_bytes = content
                tomllib.loads(content.decode("utf-8"))
            elif member.name == "zed-nix-runtime.json":
                projection_bytes = content
    if manifest_bytes is None or projection_bytes is None:
        fail("sealed artifact lacks .zpkg.toml or zed-nix-runtime.json")
    if sha256_bytes(manifest_bytes) != validate_hex(
        bridge_value.get("manifest_sha256"), "bridge manifest"
    ):
        fail("sealed manifest SHA-256 mismatch")
    if sha256_bytes(projection_bytes) != validate_hex(
        bridge_value.get("runtime_projection_sha256"),
        "bridge runtime projection",
    ):
        fail("runtime projection SHA-256 mismatch")
    projection = json.loads(projection_bytes)
    if (
        not isinstance(projection, Mapping)
        or projection.get("schema") != RUNTIME_SCHEMA
    ):
        fail("runtime projection has unsupported schema")
    if projection.get("package") != adapter.get("package"):
        fail("runtime projection package identity disagrees with canonical adapter")
    origin = projection.get("origin")
    source = adapter.get("source")
    if not isinstance(origin, Mapping) or not isinstance(source, Mapping):
        fail("runtime or canonical origin evidence is missing")
    realized = source.get("realized")
    if not isinstance(realized, Mapping):
        fail("canonical realized output evidence is missing")
    store_path = str(realized.get("store_path", ""))
    if origin.get("store_path_sha256") != sha256_bytes(
        store_path.encode("utf-8")
    ):
        fail("runtime projection store-path hash disagrees with canonical adapter")
    expected_pairs = {
        "locked_ref": source.get("locked_ref"),
        "flake_lock_sha256": source.get("flake_lock_sha256"),
        "attribute": source.get("attribute"),
        "system": realized.get("system"),
        "output": realized.get("output"),
        "derivation_json_sha256": realized.get("derivation_json_sha256"),
        "nar_hash": realized.get("nar_hash"),
        "nar_size": realized.get("nar_size"),
        "nix_version": realized.get("nix_version"),
        "store_info_json_version": realized.get("store_info_json_version"),
    }
    for key, expected in expected_pairs.items():
        if origin.get(key) != expected:
            fail(f"runtime projection disagrees with canonical adapter for {key}")
    policy = projection.get("policy")
    if not isinstance(policy, Mapping):
        fail("runtime projection lacks policy evidence")
    if (
        policy.get("nix_required_at_zed_runtime") is not False
        or policy.get("portable_reference_count") != 0
    ):
        fail("runtime projection claims Nix/runtime references are required")
    for key, expected in strict_policy().items():
        if policy.get(key) != expected:
            fail(f"runtime projection strict policy mismatch for {key}")
    return dict(bridge_value)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="nix-to-zed")
    commands = parser.add_subparsers(dest="command", required=True)

    seal_parser = commands.add_parser(
        "seal", help="seal one closure-free realized output"
    )
    seal_parser.add_argument("--store-path", type=Path, required=True)
    seal_parser.add_argument("--path-info", type=Path, required=True)
    seal_parser.add_argument("--derivation-json", type=Path, required=True)
    seal_parser.add_argument("--flake-lock", type=Path, required=True)
    seal_parser.add_argument("--locked-ref", required=True)
    seal_parser.add_argument("--attribute", required=True)
    seal_parser.add_argument("--system", required=True)
    seal_parser.add_argument("--output", required=True)
    seal_parser.add_argument("--as-package", required=True)
    seal_parser.add_argument("--target")
    seal_parser.add_argument("--bin", action="append", default=[])
    seal_parser.add_argument("--repository", required=True)
    seal_parser.add_argument("--source-revision", required=True)
    seal_parser.add_argument("--license", required=True)
    seal_parser.add_argument("--description", required=True)
    seal_parser.add_argument("--nix-version", required=True)
    seal_parser.add_argument("--store-info-json-version", type=int, default=1)
    seal_parser.add_argument("--out-dir", type=Path, required=True)
    seal_parser.add_argument("--allow-local-store", action="store_true")
    seal_parser.add_argument("--force", action="store_true")

    verify_parser = commands.add_parser(
        "verify", help="verify sealed bytes and provenance"
    )
    verify_parser.add_argument("--directory", type=Path, required=True)
    verify_parser.add_argument("--allow-local-store", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        result = seal(args) if args.command == "seal" else verify(args)
        print(json.dumps(result, indent=2, sort_keys=True))
    except SealError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    except (
        OSError,
        UnicodeDecodeError,
        json.JSONDecodeError,
        tarfile.TarError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
