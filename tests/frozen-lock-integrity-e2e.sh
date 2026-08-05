#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: tests/frozen-lock-integrity-e2e.sh /absolute/path/to/zed" >&2
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

if [[ -n "${ZED_FROZEN_LOCK_E2E_ROOT:-}" ]]; then
  suite_root=$ZED_FROZEN_LOCK_E2E_ROOT
  [[ ! -e "$suite_root" ]] || {
    echo "ZED_FROZEN_LOCK_E2E_ROOT must not already exist: $suite_root" >&2
    exit 2
  }
  mkdir -p "$suite_root"
  keep_root=true
else
  suite_root="$(mktemp -d "${TMPDIR:-/tmp}/zed-frozen-lock-integrity.XXXXXX")"
  keep_root=false
fi

cleanup() {
  if $keep_root || [[ "${ZED_FROZEN_LOCK_E2E_KEEP:-0}" == 1 ]]; then
    printf 'frozen lock integrity workspace: %s\n' "$suite_root"
  else
    rm -rf -- "$suite_root"
  fi
}
trap cleanup EXIT

fixture="$suite_root/polyglot-fixture"
registry="$suite_root/registry"
author_home="$suite_root/author-home"
valid_root="$suite_root/valid-consumer"
cases_root="$suite_root/negative-cases"
cp -R "$fixture_source" "$fixture"
mkdir -p "$registry" "$author_home" "$valid_root" "$cases_root"
registry_url="file://$registry"

fail() {
  echo "frozen lock integrity E2E: $*" >&2
  exit 1
}

file_sha256() {
  sha256sum "$1" | awk '{print $1}'
}

# Publish a real immutable fixture rather than using --skip-vcs-checks. The
# resulting consumer lock must carry genuine tag and commit provenance before
# any adversarial field mutation begins.
git -C "$fixture" init --quiet
git -C "$fixture" config user.name zed-lock-integrity-fixture
git -C "$fixture" config user.email zed-lock-integrity@users.noreply.github.com
git -C "$fixture" add --all
GIT_AUTHOR_DATE='2026-01-01T00:00:00Z' \
GIT_COMMITTER_DATE='2026-01-01T00:00:00Z' \
  git -C "$fixture" commit --quiet -m 'Create immutable polyglot fixture'
git -C "$fixture" tag v0.2.0
fixture_commit="$(git -C "$fixture" rev-parse HEAD)"

(
  cd "$fixture"
  ZED_PKG_HOME="$author_home" \
    "$zed" publish \
      --registry "$registry_url"
)

test -d "$registry/packages/zed-pkg/poly-fixture-nodejs"

cat > "$valid_root/.zpkg.toml" <<'TOML'
[package]
org = "lock-integrity"
name = "consumer"
version = "0.0.0"
description = "Frozen lock integrity fixture"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://example.invalid/lock-integrity/consumer"

[dependencies]
"zed-pkg/poly-fixture-nodejs" = "=0.2.0"

[install]
dir = "zed_modules"
adapter = "node"
TOML
printf '%s\n' '{"name":"frozen-lock-consumer","private":true}' > "$valid_root/package.json"

(
  cd "$valid_root"
  ZED_PKG_HOME="$suite_root/resolve-home" \
  ZED_PKG_REGISTRY="$registry_url" \
    "$zed" install --install-mode copy
)

valid_lock="$suite_root/valid.zpkg.lock"
valid_manifest="$suite_root/valid.zpkg.toml"
cp "$valid_root/.zpkg.lock" "$valid_lock"
cp "$valid_root/.zpkg.toml" "$valid_manifest"

python3 - "$valid_lock" "$registry_url" "$fixture_commit" <<'PY'
import re
import sys
import tomllib
from pathlib import Path

path = Path(sys.argv[1])
registry = sys.argv[2]
fixture_commit = sys.argv[3]
lock = tomllib.loads(path.read_text())
assert lock.get("version") == 1
packages = lock.get("package", [])
assert len(packages) == 1
package = packages[0]
assert package["org"] == "zed-pkg"
assert package["name"] == "poly-fixture-nodejs"
assert package["version"] == "0.2.0"
assert re.fullmatch(r"[0-9a-f]{64}", package["sha256"])
assert package["sha256"] != "0" * 64
assert isinstance(package["size"], int) and package["size"] > 0
assert package["format"] in {"tar.gz", "zip"}
assert package["vcs_tag"] == "v0.2.0"
assert package["vcs_commit"] == fixture_commit
assert re.fullmatch(r"[0-9a-f]{40}", package["vcs_commit"])
assert package["source"] == registry
PY

rm -rf -- \
  "$valid_root/zed_modules" \
  "$valid_root/node_modules" \
  "$valid_root/.zed" \
  "$valid_root/.zpkg-staging"
lock_before="$(file_sha256 "$valid_root/.zpkg.lock")"
(
  cd "$valid_root"
  ZED_PKG_HOME="$suite_root/frozen-valid-home" \
  ZED_PKG_REGISTRY="$registry_url" \
    "$zed" install --frozen --install-mode copy
)
[[ "$(file_sha256 "$valid_root/.zpkg.lock")" == "$lock_before" ]] \
  || fail "valid frozen install rewrote the lock"
[[ -f "$valid_root/zed_modules/zed-pkg/poly-fixture-nodejs/package.json" ]] \
  || fail "valid frozen install did not materialize the package"
[[ -f "$valid_root/.zed/node_path" ]] \
  || fail "valid frozen install did not write the Node adapter path"
[[ "$(cat "$valid_root/.zed/node_path")" == "zed_modules" ]] \
  || fail "valid frozen install wrote the wrong Node adapter path"
printf 'PASS: complete frozen lock restores byte-identically\n'

assert_atomic_failure() {
  local root=$1
  local label=$2
  local before
  before="$(file_sha256 "$root/.zpkg.lock")"

  if (
    cd "$root"
    ZED_PKG_HOME="$root/.isolated-zed-home" \
    ZED_PKG_REGISTRY="$registry_url" \
      "$zed" install --frozen --install-mode copy
  ) >"$root/frozen.log" 2>&1; then
    cat "$root/frozen.log" >&2
    fail "$label unexpectedly passed frozen install"
  fi

  [[ "$(file_sha256 "$root/.zpkg.lock")" == "$before" ]] \
    || fail "$label rewrote its rejected lock"
  [[ ! -e "$root/zed_modules" ]] || fail "$label materialized zed_modules"
  [[ ! -e "$root/node_modules" ]] || fail "$label materialized node_modules"
  if [[ -e "$root/.zed" ]]; then
    [[ -d "$root/.zed" && ! -L "$root/.zed" ]] \
      || fail "$label created unsafe .zed state"
    unexpected_state="$(
      find "$root/.zed" -mindepth 1 \
        ! -path "$root/.zed/operation.lock" -print -quit
    )"
    [[ -z "$unexpected_state" ]] \
      || fail "$label materialized native adapter state: $unexpected_state"
    if [[ -e "$root/.zed/operation.lock" ]]; then
      [[ -f "$root/.zed/operation.lock" && ! -L "$root/.zed/operation.lock" ]] \
        || fail "$label left an unsafe operation lock"
    fi
  fi
  if [[ -d "$root/.zpkg-staging" ]]; then
    [[ -z "$(find "$root/.zpkg-staging" -mindepth 1 -print -quit)" ]] \
      || fail "$label left transaction state"
  fi
  printf 'PASS: %s fails atomically\n' "$label"
}

new_case() {
  local name=$1
  local root="$cases_root/$name"
  mkdir -p "$root"
  cp "$valid_manifest" "$root/.zpkg.toml"
  cp "$valid_lock" "$root/.zpkg.lock"
  printf '%s\n' '{"name":"negative-lock-consumer","private":true}' > "$root/package.json"
  printf '%s\n' "$root"
}

empty_root="$(new_case empty-lock)"
printf 'version = 1\n' > "$empty_root/.zpkg.lock"
assert_atomic_failure "$empty_root" "empty version-only lock"

for field in sha256 size format vcs_tag vcs_commit source; do
  root="$(new_case "missing-$field")"
  python3 - "$root/.zpkg.lock" "$field" <<'PY'
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
field = sys.argv[2]
text = path.read_text()
pattern = re.compile(rf"^{re.escape(field)}\s*=.*(?:\n|$)", re.MULTILINE)
updated, count = pattern.subn("", text, count=1)
assert count == 1, f"generated lock did not contain {field}"
path.write_text(updated)
PY
  assert_atomic_failure "$root" "lock missing $field"
done

malformed_root="$(new_case malformed-hash)"
python3 - "$malformed_root/.zpkg.lock" <<'PY'
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
text, count = re.subn(
    r'^sha256\s*=.*$',
    'sha256 = "not-a-sha256"',
    path.read_text(),
    count=1,
    flags=re.MULTILINE,
)
assert count == 1
path.write_text(text)
PY
assert_atomic_failure "$malformed_root" "malformed artifact hash"

zero_root="$(new_case all-zero-hash)"
python3 - "$zero_root/.zpkg.lock" <<'PY'
import re
import sys
from pathlib import Path

path = Path(sys.argv[1])
text, count = re.subn(
    r'^sha256\s*=.*$',
    f'sha256 = "{"0" * 64}"',
    path.read_text(),
    count=1,
    flags=re.MULTILINE,
)
assert count == 1
path.write_text(text)
PY
assert_atomic_failure "$zero_root" "all-zero artifact hash"

drift_root="$(new_case manifest-lock-drift)"
python3 - "$drift_root/.zpkg.toml" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
text = path.read_text()
old = '"zed-pkg/poly-fixture-nodejs" = "=0.2.0"'
new = '"zed-pkg/poly-fixture-nodejs" = "=9.9.9"'
assert old in text
path.write_text(text.replace(old, new, 1))
PY
assert_atomic_failure "$drift_root" "manifest and lock requirement drift"

duplicate_root="$(new_case duplicate-package-entry)"
python3 - "$duplicate_root/.zpkg.lock" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
text = path.read_text()
marker = "[[package]]"
index = text.index(marker)
entry = text[index:]
path.write_text(text + "\n" + entry)
PY
assert_atomic_failure "$duplicate_root" "duplicate package identity"

printf 'Frozen lock integrity E2E passed.\n'
