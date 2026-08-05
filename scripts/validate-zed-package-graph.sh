#!/usr/bin/env bash
set -Eeuo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

interfaces_manifest="${1:-}"
clients_manifest="${2:-}"

[[ -f .zpkg.toml ]] || { echo 'missing .zpkg.toml' >&2; exit 1; }
for dependency in \
  '"zed-pkg/zed-clients" = "^0.1.0"' \
  '"zed-pkg/zed-interfaces" = "^0.1.0"'; do
  grep -Fq "$dependency" .zpkg.toml || { printf 'missing existing Zed dependency: %s\n' "$dependency" >&2; exit 1; }
done

grep -Fq 'dir = ".vendor/.zed"' .zpkg.toml || { echo 'Zed install directory must be .vendor/.zed' >&2; exit 1; }
grep -Fq '".vendor/.zed/**"' .zpkg.toml || { echo 'publish exclusions must omit materialized Zed dependencies' >&2; exit 1; }

if [[ -f .zpkg.lock ]] && [[ "$(wc -c < .zpkg.lock)" -le 12 ]]; then
  echo '.zpkg.lock is an empty placeholder; regenerate it with the resolver or remove it' >&2
  exit 1
fi

if grep -Fq '"zed-pkg/zed-lib"' .zpkg.toml || grep -Fq '"zed-pkg/zed-libs"' .zpkg.toml; then
  echo 'do not reference a canonical lib coordinate until that repository and package exist' >&2
  exit 1
fi

python3 - "$interfaces_manifest" "$clients_manifest" <<'PY'
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

native = cargo.get("dependencies", {}).get("zed-interfaces")
if native is None:
    errors.append("Cargo.toml must retain the native zed-interfaces dependency")

for name in manifest.get("dependencies", {}):
    package = name.lower().split("/", 1)[-1]
    if package.endswith("-infra"):
        errors.append(f"CLI must not import infrastructure package: {name}")

interfaces_path = pathlib.Path(sys.argv[1]) if sys.argv[1] else None
clients_path = pathlib.Path(sys.argv[2]) if sys.argv[2] else None

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

if errors:
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    raise SystemExit(1)
PY

printf 'zed-cli package graph validated; canonical zed-lib remains externally blocked\n'
