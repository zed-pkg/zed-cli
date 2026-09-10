#!/usr/bin/env bash
set -Eeuo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

interfaces_manifest="${1:-}"
clients_manifest="${2:-}"
lock_manifest="${3:-}"
lib_core_manifest="${4:-}"

[[ -f .zpkg.toml ]] || { echo 'missing .zpkg.toml' >&2; exit 1; }
[[ -f .cli-flags.toml ]] || { echo 'missing .cli-flags.toml' >&2; exit 1; }
for dependency in \
  '"zed-pkg/zed-clients" = "^0.1.0"' \
  '"zed-pkg/zed-interfaces" = "^0.1.0"' \
  '"zed-pkg/zed-lib-core" = "^0.1.0"' \
  '"zed-pkg/zed-lock" = "^0.1.1"'; do
  grep -Fq "$dependency" .zpkg.toml || { printf 'missing canonical Zed dependency: %s\n' "$dependency" >&2; exit 1; }
done

grep -Fq 'dir = ".vendor/.zed"' .zpkg.toml || { echo 'Zed install directory must be .vendor/.zed' >&2; exit 1; }
for output in \
  '"target/release/zed"' \
  '"target/release/zed-gitops"' \
  '"target/release/zed-git-install"'; do
  grep -Fq "$output" .zpkg.toml || { printf 'Zed package must publish required executable output: %s\n' "$output" >&2; exit 1; }
done
grep -Fq '"zed-gitops" = "target/release/zed-gitops"' .zpkg.toml || { echo 'Zed package must install the sibling zed-gitops executable' >&2; exit 1; }
grep -Fq '"zed-git-install" = "target/release/zed-git-install"' .zpkg.toml || { echo 'Zed package must install the sibling zed-git-install executable' >&2; exit 1; }
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
import re
import sys
import tomllib

root = pathlib.Path.cwd()
manifest = tomllib.loads((root / ".zpkg.toml").read_text(encoding="utf-8"))
cargo = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
# Parse the contract independently for syntax. The exact bundled flags2env
# runtime remains the semantic/audit authority elsewhere in CI.
flags_contract = tomllib.loads((root / ".cli-flags.toml").read_text(encoding="utf-8"))
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

package = manifest.get("package", {})
cargo_package = cargo.get("package", {})
repository = package.get("repository", {})
if repository.get("url") != "https://github.com/zed-pkg/zed-cli":
    errors.append("package.repository.url must point at zed-pkg/zed-cli")
if package.get("name") != cargo_package.get("name"):
    errors.append(
        f".zpkg.toml package name {package.get('name')!r} must match Cargo.toml {cargo_package.get('name')!r}"
    )
if package.get("version") != cargo_package.get("version"):
    errors.append(
        f".zpkg.toml version {package.get('version')!r} must match Cargo.toml {cargo_package.get('version')!r}"
    )
if "cli" in manifest:
    errors.append(
        ".zpkg.toml must not contain unsupported [cli] metadata; keep .cli-flags.toml as the separate executable contract"
    )

build = manifest.get("build", {})
outputs = set(build.get("outputs", []))
manifest_bins = manifest.get("bin", {})
cargo_bins = {
    entry.get("name")
    for entry in cargo.get("bin", [])
    if isinstance(entry, dict) and isinstance(entry.get("name"), str)
}
required_public_bins = {"zed", "zed-gitops", "zed-git-install"}
missing_public_bins = sorted(required_public_bins - set(manifest_bins))
if missing_public_bins:
    errors.append(
        ".zpkg.toml is missing required public binaries: " + ", ".join(missing_public_bins)
    )
for name, target in sorted(manifest_bins.items()):
    if name not in cargo_bins:
        errors.append(f".zpkg.toml public binary {name!r} is not declared by Cargo.toml")
        continue
    expected_output = f"target/release/{name}"
    if target != expected_output:
        errors.append(f".zpkg.toml [bin].{name} must map to {expected_output}")
    if expected_output not in outputs:
        errors.append(f".zpkg.toml build.outputs must retain public binary {expected_output}")

parse_contract = flags_contract.get("parse", {})
if parse_contract.get("allow_unknown") is not False:
    errors.append(".cli-flags.toml must fail closed with parse.allow_unknown = false")
env_contract = flags_contract.get("env", {})
if env_contract.get("dotenv") is not False or env_contract.get("files") != []:
    errors.append(
        ".cli-flags.toml must disable caller dotenv loading with env.dotenv = false and env.files = []"
    )
for required_global in ("no_mirrors", "trust_mirror_metadata"):
    if required_global not in flags_contract.get("flags", {}):
        errors.append(f".cli-flags.toml is missing required global flag {required_global}")

cargo_dependencies = cargo.get("dependencies", {})
flags2env = cargo_dependencies.get("flags2env")
if not isinstance(flags2env, dict):
    errors.append("Cargo.toml must retain the canonical flags2env Git dependency")
else:
    if flags2env.get("git") != "https://github.com/flags-2-env/flags-2-env.git":
        errors.append("flags2env Cargo dependency must use the canonical flags-2-env/flags-2-env repository")
    revision = flags2env.get("rev")
    if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-f]{40}", revision):
        errors.append("flags2env Cargo dependency must use an immutable lowercase 40-character revision")

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
    dependency_package = name.lower().split("/", 1)[-1]
    if dependency_package.endswith("-infra"):
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
    lib_package = lib_core.get("package", {})
    if lib_package.get("org") != "zed-pkg" or lib_package.get("name") != "zed-lib-core":
        errors.append("sibling lib-core manifest must provide zed-pkg/zed-lib-core")
    if "zed-pkg/zed-interfaces" not in lib_core.get("dependencies", {}):
        errors.append("zed-lib-core must itself depend on zed-interfaces")
    if "rust" not in lib_core.get("targets", {}):
        errors.append("zed-lib-core must retain its Rust behavior target")

if lock_path:
    lock_package = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    lock_meta = lock_package.get("package", {})
    if lock_meta.get("org") != "zed-pkg" or lock_meta.get("name") != "zed-lock":
        errors.append("sibling lock manifest must provide zed-pkg/zed-lock")
    if lock_meta.get("version") != "0.1.1":
        errors.append("sibling lock package must be hardened version 0.1.1")
    if lock_meta.get("repository", {}).get("url") != "https://github.com/zed-pkg/zed-lock":
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

printf 'zed-cli package graph, public Cargo/Zed binaries, version, and flags contract TOML validated\n'
