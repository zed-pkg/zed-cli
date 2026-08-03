#!/usr/bin/env python3
"""Fail-closed proof of concept for DEN-1411 Zed ↔ Nix adapters.

The two supported directions meet at immutable artifact boundaries:

* ``zed-to-nix`` generates a standalone flake from one already-published,
  hash-pinned Zed artifact. Zed remains the resolution authority.
* ``nix-to-zed`` seals one explicitly selected, already-realized, closure-free
  Nix output into a deterministic Zed artifact. Nix remains the resolution
  authority and ordinary Zed installation does not require Nix.

Arbitrary Zed build commands, mutable Nix references, missing locks, unresolved
Nix store references, unsafe symlinks, unknown file types, and metadata/hash
drift fail closed.
"""

from __future__ import annotations

import argparse
import base64
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
from typing import Any, BinaryIO, Iterable, Mapping, Sequence
from urllib.parse import urlparse

SCHEMA = "zed.nix-adapter/v1"
SCHEMA_VERSION = 1
DEFAULT_NIXPKGS_URL = "github:NixOS/nixpkgs/e73de5be04e0eff4190a1432b946d469c794e7b4"
DEFAULT_SYSTEMS = (
    "aarch64-darwin",
    "aarch64-linux",
    "x86_64-darwin",
    "x86_64-linux",
)
SLUG_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$")
SYSTEM_RE = re.compile(r"^[a-z0-9_+.-]+-[a-z0-9_+.-]+$")
PROGRAM_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+-]*$")
HEX_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
SRI_SHA256_RE = re.compile(r"^sha256-[A-Za-z0-9+/]{43}=$")
REV_RE = re.compile(r"^[0-9a-f]{40,64}$")
STORE_REF_RE = re.compile(rb"/nix/store/[0-9a-z]{32}-[A-Za-z0-9+._?=-]+")
RESERVED_SEALED_PATHS = {".zpkg.toml", "zed-nix-adapter.json"}


class BridgeError(ValueError):
    """Expected adapter validation failure."""


def fail(message: str) -> None:
    raise BridgeError(message)


def json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def hex_to_sri(value: str) -> str:
    validate_hex(value, "SHA-256")
    return "sha256-" + base64.b64encode(bytes.fromhex(value)).decode("ascii")


def validate_hex(value: object, label: str) -> str:
    if not isinstance(value, str) or not HEX_SHA256_RE.fullmatch(value):
        fail(f"{label} must be 64 lowercase hexadecimal SHA-256 characters")
    return value


def validate_sri(value: object, label: str) -> str:
    if not isinstance(value, str) or not SRI_SHA256_RE.fullmatch(value):
        fail(f"{label} must be a canonical SHA-256 SRI hash")
    return value


def validate_slug(value: object, label: str) -> str:
    if not isinstance(value, str) or not SLUG_RE.fullmatch(value):
        fail(f"{label} must be a lowercase Zed slug, found {value!r}")
    return value


def validate_version(value: object, label: str) -> str:
    if not isinstance(value, str) or not value or value.strip() != value:
        fail(f"{label} must be a non-empty, whitespace-free string")
    if any(character.isspace() for character in value):
        fail(f"{label} must not contain whitespace")
    return value


def validate_system(value: object) -> str:
    if not isinstance(value, str) or not SYSTEM_RE.fullmatch(value):
        fail(f"invalid Nix system {value!r}")
    return value


def validate_systems(values: Sequence[str]) -> tuple[str, ...]:
    if not values:
        fail("at least one explicit Nix system is required")
    return tuple(sorted(dict.fromkeys(validate_system(value) for value in values)))


def safe_relative(value: object, label: str) -> str:
    if not isinstance(value, str) or not value or "\0" in value or "\\" in value:
        fail(f"{label} must be a non-empty portable relative path")
    path = PurePosixPath(value)
    if path.is_absolute() or any(part in ("", ".", "..") for part in path.parts):
        fail(f"{label} escapes the package: {value!r}")
    return value


def nix_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def toml_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise BridgeError(f"missing JSON input: {path}") from error
    except json.JSONDecodeError as error:
        raise BridgeError(f"invalid JSON in {path}: {error}") from error
    if not isinstance(value, dict):
        fail(f"{path} must contain a JSON object")
    return value


def read_toml(path: Path) -> dict[str, Any]:
    try:
        with path.open("rb") as stream:
            value = tomllib.load(stream)
    except FileNotFoundError as error:
        raise BridgeError(f"missing TOML input: {path}") from error
    except tomllib.TOMLDecodeError as error:
        raise BridgeError(f"invalid TOML in {path}: {error}") from error
    if not isinstance(value, dict):
        fail(f"{path} must contain a TOML table")
    return value


def preflight_outputs(paths: Iterable[Path], force: bool) -> None:
    for path in paths:
        if path.is_symlink():
            fail(f"refusing symlink output: {path}")
        if path.exists() and not force:
            fail(f"refusing to overwrite existing output without --force: {path}")


def atomic_write(path: Path, content: bytes, *, mode: int, force: bool) -> None:
    preflight_outputs([path], force)
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
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


def write_files(root: Path, files: Mapping[str, tuple[bytes, int]], force: bool) -> None:
    if root.is_symlink():
        fail(f"refusing symlink output directory: {root}")
    targets = []
    for relative in files:
        safe_relative(relative, "generated file path")
        targets.append(root / relative)
    preflight_outputs(targets, force)
    root.mkdir(parents=True, exist_ok=True)
    for relative, (content, mode) in sorted(files.items()):
        atomic_write(root / relative, content, mode=mode, force=force)


def package_identity(manifest: Mapping[str, Any]) -> tuple[str, str, str, Mapping[str, Any]]:
    package = manifest.get("package")
    if not isinstance(package, Mapping):
        fail(".zpkg.toml must contain [package]")
    repository = package.get("repository")
    if not isinstance(repository, Mapping):
        fail(".zpkg.toml must contain [package.repository]")
    return (
        validate_slug(package.get("org"), "[package].org"),
        validate_slug(package.get("name"), "[package].name"),
        validate_version(package.get("version"), "[package].version"),
        repository,
    )


def collect_bins(manifest: Mapping[str, Any]) -> dict[str, str]:
    raw = manifest.get("bin", {})
    if raw is None:
        return {}
    if not isinstance(raw, Mapping):
        fail("[bin] must be a TOML table")
    result: dict[str, str] = {}
    for name, path in raw.items():
        if not isinstance(name, str) or not PROGRAM_RE.fullmatch(name):
            fail(f"invalid Zed binary name {name!r}")
        result[name] = safe_relative(path, f"[bin].{name}")
    return dict(sorted(result.items()))


def ensure_dependency_free(manifest: Mapping[str, Any]) -> None:
    for key in ("dependencies", "build-dependencies", "build_dependencies"):
        value = manifest.get(key)
        if isinstance(value, Mapping) and value:
            fail(
                "first-generation Zed→Nix export supports dependency-free artifacts only; "
                "exporting a frozen .zpkg.lock graph is the next adapter slice"
            )
    if manifest.get("build") is not None:
        fail(
            "strict artifact export refuses arbitrary [build] commands; publish the "
            "already-built bytes or add a reviewed typed stack adapter"
        )


def validate_artifact_url(value: object, allow_local: bool) -> str:
    if not isinstance(value, str) or not value:
        fail("version metadata download_url must be a non-empty URL")
    parsed = urlparse(value)
    if parsed.scheme == "https" and parsed.netloc:
        return value
    if allow_local and parsed.scheme == "file" and parsed.path:
        return value
    if allow_local and parsed.scheme == "http" and parsed.hostname in {
        "127.0.0.1",
        "localhost",
        "::1",
    }:
        return value
    fail("published artifacts require HTTPS; local fixtures require --allow-local-source")


def validate_nixpkgs_lock(path: Path, expected_url: str) -> tuple[bytes, str, str]:
    content = path.read_bytes()
    value = read_json(path)
    nodes = value.get("nodes")
    if not isinstance(nodes, Mapping):
        fail("Nix lock has no nodes object")
    root_name = value.get("root")
    root = nodes.get(root_name) if isinstance(root_name, str) else None
    if not isinstance(root, Mapping):
        fail("Nix lock has no valid root node")
    inputs = root.get("inputs")
    nixpkgs_node_name = inputs.get("nixpkgs") if isinstance(inputs, Mapping) else None
    nixpkgs = nodes.get(nixpkgs_node_name) if isinstance(nixpkgs_node_name, str) else None
    locked = nixpkgs.get("locked") if isinstance(nixpkgs, Mapping) else None
    if not isinstance(locked, Mapping):
        fail("Nix lock has no locked nixpkgs input")
    revision = locked.get("rev")
    nar_hash = locked.get("narHash")
    if not isinstance(revision, str) or not REV_RE.fullmatch(revision):
        fail("Nix lock nixpkgs revision is not immutable")
    validate_sri(nar_hash, "Nix lock nixpkgs narHash")
    if revision not in expected_url:
        fail("--nixpkgs-url revision does not match the supplied lock")
    return content, revision, nar_hash
