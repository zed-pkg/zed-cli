#!/usr/bin/env bash
set -Eeuo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

interfaces_manifest="${1:-}"
clients_manifest="${2:-}"
lock_manifest="${3:-}"
lib_core_manifest="${4:-}"

[[ -f .zpkg.toml ]] || { echo 'missing .zpkg.toml' >&2; exit 1; }
for dependency in \
  '"zed-pkg/zed-clients" = "^0.1.0"' \
  '"zed-pkg/zed-interfaces" = "^0.1.0"' \
  '"zed-pkg/zed-lib-core" = "^0.1.0"' \
  '"zed-pkg/zed-lock" = "^0.1.1"'; do
  grep -Fq "$dependency" .zpkg.toml || { printf 'missing canonical Zed dependency: %s\n' "$dependency" >&2; exit 1; }
done

grep -Fq 'dir = ".vendor/.zed"' .zpkg.toml || { echo 'Zed install directory must be .vendor/.zed' >&2; exit 1; }
grep -Fq 'outputs = ["target/release/zed", "target/release/zed-gitops"]' .zpkg.toml || { echo 'Zed package must publish both root and GitOps executables' >&2; exit 1; }
grep -Fq '"zed-gitops" = "target/release/zed-gitops"' .zpkg.toml || { echo 'Zed package must install the sibling zed-gitops executable' >&2; exit 1; }
grep -Fq '".vendor/.zed/**"' .zpkg.toml || { echo 'publish exclusions must omit materialized Zed dependencies' >&2; exit 1; }

if [[ -f .zpkg.lock ]] && [[ "$(wc -c < .zpkg.lock)" -le 12 ]]; then
  echo '.zpkg.lock is an empty placeholder; regenerate it with the resolver or remove it' >&2
  exit 1
fi

if [[ -d crates/zed-lock ]]; then
  echo 'the extracted lock crate must remain independently owned; do not restore crates/zed-lock' >&2
  exit 1
fi

if grep -Fq '"zed-pkg/zed-lib"' .zpkg.toml || grep -Fq '"zed-pkg/zed-libs"' .zpkg.toml; then
  echo 'legacy zed-lib coordinates are forbidden; use zed-pkg/zed-lib-core' >&2
  exit 1
fi

python3 - "$interfaces_manifest" "$clients_manifest" "$lock_manifest" "$lib_core_manifest" <<'PY'
from __future__ import annotations

import pathlib
import sys
import tomllib

root = pathlib.Path.cwd()
manifest = tomllib.loads((root / ".zpkg.toml").read_text(encoding="utf-8"))
cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
cargo_lock = (root / "Cargo.lock").read_text(encoding="utf-8")
errors: list[str] = []

expected_sources = {
    "zed-interfaces": (
        "https://github.com/zed-pkg/zed-interfaces.git",
        "3c54298fc7a8c1b2f9c1d74f588c6118b38f197e",
    ),
    "zed-client": (
        "https://github.com/zed-pkg/zed-clients.git",
        "b32e089caea772f166204fb1c7bcaad6f56942fe",
    ),
    "zed-lib": (
        "https://github.com/zed-pkg/zed-lib-core.git",
        "eac0878750332b031bc12f6040b6a795a17e7417",
    ),
    "zed-lock": (
        "https://github.com/zed-pkg/zed-lock.git",
        "1db0da00d30fcf2e0762f50eedb1f88458020b52",
    ),
}

repository = manifest.get("package", {}).get("repository", {})
if repository.get("url") != "https://github.com/zed-pkg/zed-cli":
    errors.append("package.repository.url must point at zed-pkg/zed-cli")

cargo_dependencies = cargo.get("dependencies", {})
for dependency, (repository_url, revision) in expected_sources.items():
    native = cargo_dependencies.get(dependency)
    if not isinstance(native, dict):
        errors.append(f"Cargo.toml must retain the native {dependency} Git dependency")
        continue
    if native.get("git") != repository_url:
        errors.append(f"{dependency} Cargo dependency must use {repository_url}")
    if native.get("rev") != revision:
        errors.append(f"{dependency} Cargo dependency must pin {revision}")

    source = f"git+{repository_url}?rev={revision}#{revision}"
    if source not in cargo_lock:
        errors.append(f"Cargo.lock must resolve the exact {dependency} revision")

if 'name = "zed-lock"\nversion = "0.1.1"' not in cargo_lock:
    errors.append("Cargo.lock must resolve zed-lock version 0.1.1")

for name in manifest.get("dependencies", {}):
    package = name.lower().split("/", 1)[-1]
    if package.endswith("-infra"):
        errors.append(f"CLI must not import infrastructure package: {name}")

interfaces_path = pathlib.Path(sys.argv[1]) if sys.argv[1] else None
clients_path = pathlib.Path(sys.argv[2]) if sys.argv[2] else None
lock_path = pathlib.Path(sys.argv[3]) if sys.argv[3] else None
lib_core_path = pathlib.Path(sys.argv[4]) if sys.argv[4] else None

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

if lib_core_path:
    lib_core = tomllib.loads(lib_core_path.read_text(encoding="utf-8"))
    package = lib_core.get("package", {})
    if package.get("org") != "zed-pkg" or package.get("name") != "zed-lib-core":
        errors.append("sibling lib-core manifest must provide zed-pkg/zed-lib-core")
    if "zed-pkg/zed-interfaces" not in lib_core.get("dependencies", {}):
        errors.append("zed-lib-core must itself depend on zed-interfaces")
    if "rust" not in lib_core.get("targets", {}):
        errors.append("zed-lib-core must retain its Rust behavior target")

if lock_path:
    lock_package = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    package = lock_package.get("package", {})
    if package.get("org") != "zed-pkg" or package.get("name") != "zed-lock":
        errors.append("sibling lock manifest must provide zed-pkg/zed-lock")
    if package.get("version") != "0.1.1":
        errors.append("sibling lock package must be hardened version 0.1.1")
    if package.get("repository", {}).get("url") != "https://github.com/zed-pkg/zed-lock":
        errors.append("sibling lock package must declare the canonical repository")
    rust_target = lock_package.get("targets", {}).get("rust", {})
    if rust_target.get("dir") != ".":
        errors.append("zed-lock must publish its root Rust target")
    if rust_target.get("adapter") != "rust":
        errors.append("zed-lock root target must retain the Rust adapter")
    if "native" in rust_target:
        errors.append(
            "zed-lock root target must not declare native release metadata: "
            "the root is the canonical Zed repository package, while cargo publish "
            "remains an independent crates.io release operation"
        )
    sibling_placeholder = lock_path.parent / ".zpkg.lock"
    if sibling_placeholder.exists():
        errors.append("dependency-free zed-lock must not restore a placeholder .zpkg.lock")

if errors:
    for error in errors:
        print(f"error: {error}", file=sys.stderr)
    raise SystemExit(1)
PY

printf 'zed-cli package graph validated across exact zed-client, zed-interfaces, zed-lib-core, and zed-lock revisions\n'