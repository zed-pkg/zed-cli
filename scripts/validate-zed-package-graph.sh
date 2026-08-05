#!/usr/bin/env bash
set -Eeuo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

interfaces_manifest="${1:-}"
clients_manifest="${2:-}"
lock_manifest="${3:-}"

readonly lock_commit='a0dc78d385bc3ab553d3027b427f5f1428239c9c'
readonly lock_version='0.1.1'

[[ -f .zpkg.toml ]] || { echo 'missing .zpkg.toml' >&2; exit 1; }
for dependency in \
  '"zed-pkg/zed-clients" = "^0.1.0"' \
  '"zed-pkg/zed-interfaces" = "^0.1.0"' \
  '"zed-pkg/zed-lock" = "^0.1.1"'; do
  grep -Fq "$dependency" .zpkg.toml || {
    printf 'missing canonical Zed dependency: %s\n' "$dependency" >&2
    exit 1
  }
done

grep -Fq 'dir = ".vendor/.zed"' .zpkg.toml || {
  echo 'Zed install directory must be .vendor/.zed' >&2
  exit 1
}
grep -Fq '".vendor/.zed/**"' .zpkg.toml || {
  echo 'publish exclusions must omit materialized Zed dependencies' >&2
  exit 1
}

if grep -Fq '"zed-pkg/zed-lib"' .zpkg.toml \
  || grep -Fq '"zed-pkg/zed-libs"' .zpkg.toml; then
  echo 'do not invent an umbrella zed-lib coordinate; import concrete packages' >&2
  exit 1
fi

python3 - \
  "$interfaces_manifest" \
  "$clients_manifest" \
  "$lock_manifest" \
  "$lock_commit" \
  "$lock_version" <<'PY'
from __future__ import annotations

import pathlib
import sys
import tomllib

root = pathlib.Path.cwd()
interfaces_arg, clients_arg, lock_arg, lock_commit, lock_version = sys.argv[1:]
manifest = tomllib.loads((root / ".zpkg.toml").read_text(encoding="utf-8"))
cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
cargo_lock = tomllib.loads((root / "Cargo.lock").read_text(encoding="utf-8"))
errors: list[str] = []


def normalized_placeholder(path: pathlib.Path) -> bool:
    if not path.is_file():
        return False
    return (
        path.read_text(encoding="utf-8").replace("\r\n", "\n").strip()
        == "version = 1"
    )


repository = manifest.get("package", {}).get("repository", {})
if repository.get("url") != "https://github.com/zed-pkg/zed-cli":
    errors.append("package.repository.url must point at zed-pkg/zed-cli")

cargo_dependencies = cargo.get("dependencies", {})
if cargo_dependencies.get("zed-interfaces") is None:
    errors.append("Cargo.toml must retain the native zed-interfaces dependency")

native_lock = cargo_dependencies.get("zed-lock")
if not isinstance(native_lock, dict):
    errors.append("Cargo.toml must use the standalone zed-lock git dependency")
else:
    if native_lock.get("git") != "https://github.com/zed-pkg/zed-lock.git":
        errors.append("Cargo zed-lock.git must point at zed-pkg/zed-lock")
    if native_lock.get("rev") != lock_commit:
        errors.append(
            f"Cargo zed-lock.rev must pin hardened merge commit {lock_commit}"
        )

if (root / "crates/zed-lock").exists():
    errors.append("the removed internal crates/zed-lock copy must not return")

lock_entries = [
    package
    for package in cargo_lock.get("package", [])
    if package.get("name") == "zed-lock"
]
if len(lock_entries) != 1:
    errors.append(f"Cargo.lock must contain exactly one zed-lock entry, got {len(lock_entries)}")
else:
    entry = lock_entries[0]
    if entry.get("version") != lock_version:
        errors.append(
            f"Cargo.lock zed-lock version must be {lock_version}, got {entry.get('version')!r}"
        )
    expected_source = (
        "git+https://github.com/zed-pkg/zed-lock.git"
        f"?rev={lock_commit}#{lock_commit}"
    )
    if entry.get("source") != expected_source:
        errors.append(
            "Cargo.lock zed-lock source must resolve the exact hardened merge commit"
        )

for name in manifest.get("dependencies", {}):
    package = name.lower().split("/", 1)[-1]
    if package.endswith("-infra"):
        errors.append(f"CLI must not import infrastructure package: {name}")

if normalized_placeholder(root / ".zpkg.lock"):
    errors.append(
        ".zpkg.lock is an empty placeholder; regenerate it with the resolver or remove it"
    )

interfaces_path = pathlib.Path(interfaces_arg) if interfaces_arg else None
clients_path = pathlib.Path(clients_arg) if clients_arg else None
lock_path = pathlib.Path(lock_arg) if lock_arg else None

if interfaces_path:
    interfaces = tomllib.loads(interfaces_path.read_text(encoding="utf-8"))
    if interfaces.get("package", {}).get("name") != "zed-interfaces":
        errors.append("sibling interfaces manifest does not provide zed-interfaces")

if clients_path:
    clients = tomllib.loads(clients_path.read_text(encoding="utf-8"))
    if clients.get("package", {}).get("name") != "zed-clients":
        errors.append("sibling clients manifest does not provide zed-clients")
    client_dependencies = clients.get("dependencies", {})
    if "zed-pkg/zed-interfaces" not in client_dependencies:
        errors.append("zed-clients must itself depend on zed-interfaces")
    if "rust" not in clients.get("targets", {}):
        errors.append("zed-clients must retain its Rust SDK target")

if lock_path:
    external = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    package = external.get("package", {})
    expected = {
        "org": "zed-pkg",
        "name": "zed-lock",
        "version": lock_version,
        "license": "MIT",
        "language": "rust",
    }
    for key, value in expected.items():
        if package.get(key) != value:
            errors.append(
                f"external zed-lock package.{key} must be {value!r}, got {package.get(key)!r}"
            )
    external_repository = package.get("repository", {})
    if external_repository.get("url") != "https://github.com/zed-pkg/zed-lock":
        errors.append("external zed-lock repository URL is not canonical")

    rust_target = external.get("targets", {}).get("rust")
    if not isinstance(rust_target, dict):
        errors.append("external zed-lock must expose targets.rust")
    else:
        if rust_target.get("dir") != "." or rust_target.get("adapter") != "rust":
            errors.append("external zed-lock Rust target must own the repository root")
        native = rust_target.get("native", {})
        if native.get("registry") != "crates-io" or native.get("package") != "zed-lock":
            errors.append("external zed-lock native route must be crates-io/zed-lock")

    external_root = lock_path.parent
    for required in (
        "Cargo.toml",
        "CHANGELOG.md",
        "PROVENANCE.md",
        "SECURITY.md",
        "scripts/check-package-contract.py",
    ):
        if not (external_root / required).is_file():
            errors.append(f"external zed-lock is missing {required}")
    if normalized_placeholder(external_root / ".zpkg.lock"):
        errors.append("external zed-lock must not ship an empty placeholder lock")
else:
    print(
        "warning: no external zed-lock manifest supplied; repository metadata checks skipped",
        file=sys.stderr,
    )

if errors:
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    raise SystemExit(1)
PY

printf 'zed-cli package graph validated with hardened zed-lock v0.1.1\n'
