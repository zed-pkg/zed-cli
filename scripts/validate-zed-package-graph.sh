#!/usr/bin/env bash
set -Eeuo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

interfaces_manifest="${1:-}"
clients_manifest="${2:-}"
lock_manifest="${3:-}"

[[ -f .zpkg.toml ]] || { echo 'missing .zpkg.toml' >&2; exit 1; }
for dependency in \
  '"zed-pkg/zed-clients" = "^0.1.0"' \
  '"zed-pkg/zed-interfaces" = "^0.1.0"' \
  '"zed-pkg/zed-lock" = "^0.1.0"'; do
  grep -Fq "$dependency" .zpkg.toml || { printf 'missing canonical Zed dependency: %s\n' "$dependency" >&2; exit 1; }
done

grep -Fq 'dir = ".vendor/.zed"' .zpkg.toml || { echo 'Zed install directory must be .vendor/.zed' >&2; exit 1; }
grep -Fq '".vendor/.zed/**"' .zpkg.toml || { echo 'publish exclusions must omit materialized Zed dependencies' >&2; exit 1; }

if [[ -f .zpkg.lock ]] && [[ "$(wc -c < .zpkg.lock)" -le 12 ]]; then
  echo '.zpkg.lock is an empty placeholder; regenerate it with the resolver or remove it' >&2
  exit 1
fi

if grep -Fq '"zed-pkg/zed-lib"' .zpkg.toml || grep -Fq '"zed-pkg/zed-libs"' .zpkg.toml; then
  echo 'do not invent an umbrella zed-lib coordinate; import the concrete zed-lock package' >&2
  exit 1
fi

python3 - "$interfaces_manifest" "$clients_manifest" "$lock_manifest" <<'PY'
from __future__ import annotations

import pathlib
import sys
import tomllib

root = pathlib.Path.cwd()
manifest = tomllib.loads((root / ".zpkg.toml").read_text(encoding="utf-8"))
cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
errors: list[str] = []

repository = manifest.get("package", {}).get("repository", {})
if repository.get("url") != "https://github.com/zed-pkg/zed-cli":
    errors.append("package.repository.url must point at zed-pkg/zed-cli")

native_interfaces = cargo.get("dependencies", {}).get("zed-interfaces")
if native_interfaces is None:
    errors.append("Cargo.toml must retain the native zed-interfaces dependency")

native_lock = cargo.get("dependencies", {}).get("zed-lock")
if not isinstance(native_lock, dict) or native_lock.get("path") != "crates/zed-lock":
    errors.append(
        "the transitional Cargo edge must retain crates/zed-lock until the external source pin lands"
    )

for name in manifest.get("dependencies", {}):
    package = name.lower().split("/", 1)[-1]
    if package.endswith("-infra"):
        errors.append(f"CLI must not import infrastructure package: {name}")

interfaces_path = pathlib.Path(sys.argv[1]) if sys.argv[1] else None
clients_path = pathlib.Path(sys.argv[2]) if sys.argv[2] else None
lock_path = pathlib.Path(sys.argv[3]) if sys.argv[3] else None

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


def comparable_files(package_root: pathlib.Path) -> dict[str, bytes]:
    selected: dict[str, bytes] = {}
    for relative in ("Cargo.toml", "LICENSE", "SECURITY.md"):
        path = package_root / relative
        if not path.is_file():
            errors.append(f"missing lock package file: {path}")
            continue
        selected[relative] = path.read_bytes()
    for directory in ("src", "tests", "examples"):
        base = package_root / directory
        if not base.is_dir():
            errors.append(f"missing lock package directory: {base}")
            continue
        for path in sorted(base.rglob("*")):
            if path.is_file():
                selected[path.relative_to(package_root).as_posix()] = path.read_bytes()
    return selected


if lock_path:
    lock_manifest = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    lock_package = lock_manifest.get("package", {})
    if lock_package.get("org") != "zed-pkg":
        errors.append("sibling lock manifest must use package.org = zed-pkg")
    if lock_package.get("name") != "zed-lock":
        errors.append("sibling lock manifest does not provide zed-lock")
    if lock_package.get("version") != "0.1.0":
        errors.append("sibling lock manifest must expose version 0.1.0")
    lock_repository = lock_package.get("repository", {})
    if lock_repository.get("url") != "https://github.com/zed-pkg/zed-lock":
        errors.append("sibling lock manifest must point at zed-pkg/zed-lock")

    lock_targets = lock_manifest.get("targets", {})
    rust_target = lock_targets.get("rust")
    if not isinstance(rust_target, dict):
        errors.append("zed-lock must expose its Rust package as targets.rust")
    else:
        if rust_target.get("dir") != "." or rust_target.get("adapter") != "rust":
            errors.append("zed-lock targets.rust must own the repository root with the Rust adapter")
        native = rust_target.get("native", {})
        if native.get("registry") != "crates-io" or native.get("package") != "zed-lock":
            errors.append("zed-lock must declare the canonical crates-io/zed-lock native route")

    lock_root = lock_path.parent
    placeholder = lock_root / ".zpkg.lock"
    if placeholder.exists() and placeholder.stat().st_size <= 12:
        errors.append("zed-lock must not ship an empty placeholder .zpkg.lock")

    internal_files = comparable_files(root / "crates/zed-lock")
    external_files = comparable_files(lock_root)
    if internal_files.keys() != external_files.keys():
        missing_external = sorted(internal_files.keys() - external_files.keys())
        missing_internal = sorted(external_files.keys() - internal_files.keys())
        errors.append(
            "zed-lock source inventory differs between CLI and standalone package; "
            f"missing externally={missing_external}, missing internally={missing_internal}"
        )
    else:
        for relative in internal_files:
            if internal_files[relative] != external_files[relative]:
                errors.append(
                    f"zed-lock source drift at {relative}; reconcile semantically before switching Cargo authority"
                )
else:
    print(
        "warning: no external zed-lock manifest supplied; cross-repository package and source checks skipped",
        file=sys.stderr,
    )

if errors:
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    raise SystemExit(1)
PY

printf 'zed-cli package graph validated with canonical zed-lock source parity\n'
