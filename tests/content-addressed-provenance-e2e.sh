#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: tests/content-addressed-provenance-e2e.sh /absolute/path/to/zed" >&2
  exit 2
fi

zed=$1
if [[ ! -x "$zed" ]]; then
  echo "zed executable not found: $zed" >&2
  exit 2
fi
zed="$(cd -- "$(dirname -- "$zed")" && pwd -P)/$(basename -- "$zed")"

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
fixture_source="$repo_root/tests/fixtures/polyglot"
[[ -f "$fixture_source/.zpkg.toml" ]] || {
  echo "polyglot fixture not found: $fixture_source" >&2
  exit 2
}

if [[ -n "${ZED_CONTENT_PROVENANCE_E2E_ROOT:-}" ]]; then
  suite_root=$ZED_CONTENT_PROVENANCE_E2E_ROOT
  [[ ! -e "$suite_root" ]] || {
    echo "ZED_CONTENT_PROVENANCE_E2E_ROOT must not already exist: $suite_root" >&2
    exit 2
  }
  mkdir -p "$suite_root"
  keep_root=true
else
  suite_root="$(mktemp -d "${TMPDIR:-/tmp}/zed-content-provenance.XXXXXX")"
  keep_root=false
fi

cleanup() {
  if $keep_root || [[ "${ZED_CONTENT_PROVENANCE_E2E_KEEP:-0}" == 1 ]]; then
    printf 'content-addressed provenance workspace: %s\n' "$suite_root"
  else
    rm -rf -- "$suite_root"
  fi
}
trap cleanup EXIT

fixture="$suite_root/unverified-fixture"
registry="$suite_root/registry"
author_home="$suite_root/author-home"
consumer="$suite_root/consumer"
registry_url="file://$registry"
cp -R "$fixture_source" "$fixture"
rm -rf -- "$fixture/.git"
mkdir -p "$registry" "$author_home" "$consumer"

fail() {
  echo "content-addressed provenance E2E: $*" >&2
  exit 1
}

file_sha256() {
  sha256sum "$1" | awk '{print $1}'
}

# Explicitly exercise the legacy/VCS-skipped publication path. The package has
# exact artifact identity but no source revision. The canonical lock writer
# must turn that in-memory absence into artifact-sha256:<digest>; it must not
# emit an incomplete lock or invent a Git commit.
(
  cd "$fixture"
  ZED_PKG_HOME="$author_home" \
    "$zed" publish \
      --registry "$registry_url" \
      --skip-vcs-checks
)

cat > "$consumer/.zpkg.toml" <<'TOML'
[package]
org = "lock-integrity"
name = "content-addressed-consumer"
version = "0.0.0"
description = "Content-addressed provenance fixture"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://example.invalid/lock-integrity/content-addressed-consumer"

[dependencies]
"zed-pkg/poly-fixture-nodejs" = "=0.2.0"

[install]
dir = "zed_modules"
adapter = "node"
TOML
printf '%s\n' '{"name":"content-addressed-consumer","private":true}' \
  > "$consumer/package.json"

(
  cd "$consumer"
  ZED_PKG_HOME="$suite_root/resolve-home" \
  ZED_PKG_REGISTRY="$registry_url" \
    "$zed" install --install-mode copy
)

python3 - "$consumer/.zpkg.lock" "$registry_url" <<'PY'
import re
import sys
import tomllib
from pathlib import Path

lock_path = Path(sys.argv[1])
registry = sys.argv[2]
lock = tomllib.loads(lock_path.read_text())
assert lock.get("version") == 1
packages = lock.get("package", [])
assert len(packages) == 1
package = packages[0]
assert package["org"] == "zed-pkg"
assert package["name"] == "poly-fixture-nodejs"
assert package["version"] == "0.2.0"
assert re.fullmatch(r"[0-9a-f]{64}", package["sha256"])
assert package["sha256"] != "0" * 64
assert package["size"] > 0
assert package["format"] in {"tar.gz", "zip"}
assert package["vcs_tag"] == "v0.2.0"
assert package["vcs_commit"] == f"artifact-sha256:{package['sha256']}"
assert package["source"] == registry
PY

lock_before="$(file_sha256 "$consumer/.zpkg.lock")"
rm -rf -- \
  "$consumer/zed_modules" \
  "$consumer/node_modules" \
  "$consumer/.zed" \
  "$consumer/.zpkg-staging"

(
  cd "$consumer"
  ZED_PKG_HOME="$suite_root/frozen-home" \
  ZED_PKG_REGISTRY="$registry_url" \
    "$zed" install --frozen --install-mode copy
)

[[ "$(file_sha256 "$consumer/.zpkg.lock")" == "$lock_before" ]] \
  || fail "frozen replay rewrote the content-addressed lock"
[[ -f "$consumer/zed_modules/zed-pkg/poly-fixture-nodejs/package.json" ]] \
  || fail "frozen replay did not materialize the package"
[[ -f "$consumer/.zed/node_path" ]] \
  || fail "frozen replay did not materialize Node adapter state"
[[ "$(cat "$consumer/.zed/node_path")" == "zed_modules" ]] \
  || fail "frozen replay wrote an unexpected Node adapter path"
[[ ! -e "$consumer/.zpkg-staging" ]] \
  || fail "successful replay left staging state"

printf 'PASS: VCS-skipped publication emits and replays explicit artifact-sha256 provenance.\n'
