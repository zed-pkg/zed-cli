#!/usr/bin/env bash
set -euo pipefail

zed="${1:-}"
if [[ -z "$zed" ]]; then
  echo "usage: $0 /absolute/path/to/zed" >&2
  exit 64
fi
if [[ ! -x "$zed" ]]; then
  echo "zed executable is not executable: $zed" >&2
  exit 66
fi
zed="$(cd "$(dirname "$zed")" && pwd)/$(basename "$zed")"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root="${ZED_DURABLE_E2E_ROOT:-}"
keep="${ZED_DURABLE_E2E_KEEP:-0}"
if [[ -z "$root" ]]; then
  root="$(mktemp -d "${TMPDIR:-/tmp}/zed-durable-first-install.XXXXXX")"
fi
mkdir -p "$root"
if [[ "$keep" != "1" ]]; then
  trap 'rm -rf "$root"' EXIT
fi

registry="$root/registry"
home="$root/home"
fixtures="$root/fixtures"
mkdir -p "$registry" "$home" "$fixtures"

copy_consumer() {
  local destination="$1"
  mkdir -p "$destination/src/deep"
  printf '{"private":true,"name":"durable-consumer"}\n' > "$destination/package.json"
}

publish_fixture() {
  local source="$1"
  (
    cd "$source"
    "$zed" publish \
      --registry "file://$registry" \
      --skip-vcs-checks
  )
}

fixture_one="$fixtures/node-lib-one"
fixture_two="$fixtures/node-lib-two"
cp -R "$repo_root/tests/fixtures/docker-install/node-lib" "$fixture_one"
cp -R "$repo_root/tests/fixtures/docker-install/node-lib" "$fixture_two"
python3 - "$fixture_two" <<'PY'
from pathlib import Path
import sys

root = Path(sys.argv[1])
manifest = root / ".zpkg.toml"
package = root / "package.json"
manifest.write_text(
    manifest.read_text().replace(
        'name = "docker-node-lib"',
        'name = "docker-node-lib-two"',
        1,
    )
)
package.write_text(
    package.read_text().replace(
        '"name": "@zed-pkg/docker-node-lib"',
        '"name": "@zed-pkg/docker-node-lib-two"',
        1,
    )
)
PY

publish_fixture "$fixture_one"
publish_fixture "$fixture_two"

# A normal first install creates durable project intent and can be extended by
# a second package-bearing install while the generated identity is still in
# place.
durable="$root/durable"
copy_consumer "$durable"
(
  cd "$durable/src/deep"
  "$zed" install zed-pkg/docker-node-lib@^1 \
    --registry "file://$registry" \
    --home "$home/durable" \
    --install-mode copy
)
test -f "$durable/.zpkg.toml"
test -f "$durable/.zpkg.lock"
grep -F 'org = "zed-local"' "$durable/.zpkg.toml"
grep -F 'zed-generated-consumer' "$durable/.zpkg.toml"
grep -F '"zed-pkg/docker-node-lib" = "^1"' "$durable/.zpkg.toml"
grep -F 'adapter = "node"' "$durable/.zpkg.toml"
test -d "$durable/zed_modules/zed-pkg/docker-node-lib"
test -d "$durable/node_modules/@zed-pkg/docker-node-lib"

(
  cd "$durable"
  "$zed" install zed-pkg/docker-node-lib-two@^1 \
    --registry "file://$registry" \
    --home "$home/durable" \
    --install-mode copy
)
grep -F '"zed-pkg/docker-node-lib" = "^1"' "$durable/.zpkg.toml"
grep -F '"zed-pkg/docker-node-lib-two" = "^1"' "$durable/.zpkg.toml"
test -d "$durable/zed_modules/zed-pkg/docker-node-lib-two"
test -d "$durable/node_modules/@zed-pkg/docker-node-lib-two"

# A generated consumer cannot be published accidentally, even when normal VCS
# provenance checks are explicitly skipped.
if (
  cd "$durable"
  "$zed" publish \
    --registry "file://$registry" \
    --dry-run \
    --skip-vcs-checks
) >"$root/generated-publish.log" 2>&1; then
  echo "generated consumer manifest unexpectedly published" >&2
  exit 1
fi
grep -F 'auto-generated local consumer manifest' "$root/generated-publish.log"

# Conflicting requirements fail before replacing the generated manifest.
cp "$durable/.zpkg.toml" "$root/durable-manifest.before"
if (
  cd "$durable"
  "$zed" install zed-pkg/docker-node-lib@^2 \
    --registry "file://$registry" \
    --home "$home/durable" \
    --install-mode copy
) >"$root/conflict.log" 2>&1; then
  echo "conflicting generated-manifest requirement unexpectedly succeeded" >&2
  exit 1
fi
cmp "$root/durable-manifest.before" "$durable/.zpkg.toml"
grep -F 'conflicts with `^2`' "$root/conflict.log"

# Two true first installs serialize on the project-scoped manifest lock and
# retain both direct dependencies rather than choosing a winner.
concurrent="$root/concurrent"
copy_consumer "$concurrent"
(
  cd "$concurrent"
  "$zed" install zed-pkg/docker-node-lib@^1 \
    --registry "file://$registry" \
    --home "$home/concurrent" \
    --install-mode copy
) >"$root/concurrent-one.log" 2>&1 &
pid_one=$!
(
  cd "$concurrent"
  "$zed" install zed-pkg/docker-node-lib-two@^1 \
    --registry "file://$registry" \
    --home "$home/concurrent" \
    --install-mode copy
) >"$root/concurrent-two.log" 2>&1 &
pid_two=$!

failed=0
if ! wait "$pid_one"; then
  failed=1
fi
if ! wait "$pid_two"; then
  failed=1
fi
if [[ "$failed" == "1" ]]; then
  cat "$root/concurrent-one.log" >&2
  cat "$root/concurrent-two.log" >&2
  exit 1
fi

test -f "$concurrent/.zpkg.toml"
test -f "$concurrent/.zpkg.lock"
grep -F '"zed-pkg/docker-node-lib" = "^1"' "$concurrent/.zpkg.toml"
grep -F '"zed-pkg/docker-node-lib-two" = "^1"' "$concurrent/.zpkg.toml"
test -d "$concurrent/zed_modules/zed-pkg/docker-node-lib"
test -d "$concurrent/zed_modules/zed-pkg/docker-node-lib-two"

# Resolution/install failures remove only the exact generated manifest and do
# not leave a lockfile or materialized dependency tree behind.
failed_root="$root/failed"
copy_consumer "$failed_root"
if (
  cd "$failed_root"
  "$zed" install zed-pkg/docker-node-lib@=9.9.9 \
    --registry "file://$registry" \
    --home "$home/failed" \
    --install-mode copy
) >"$root/failed-install.log" 2>&1; then
  echo "missing version unexpectedly installed" >&2
  exit 1
fi
test ! -e "$failed_root/.zpkg.toml"
test ! -e "$failed_root/.zpkg.lock"
test ! -e "$failed_root/zed_modules"
test ! -e "$failed_root/node_modules/@zed-pkg/docker-node-lib"

# The canonical flag and environment variable preserve the established
# synthetic-manifest workflow without prompting or writing .zpkg.toml.
ephemeral="$root/ephemeral"
copy_consumer "$ephemeral"
(
  cd "$ephemeral"
  "$zed" install zed-pkg/docker-node-lib@^1 \
    --do-not-write-new-manifest \
    --registry "file://$registry" \
    --home "$home/ephemeral" \
    --install-mode copy
)
test ! -e "$ephemeral/.zpkg.toml"
test -f "$ephemeral/.zpkg.lock"

env_ephemeral="$root/env-ephemeral"
copy_consumer "$env_ephemeral"
(
  cd "$env_ephemeral"
  ZED_PKG_DO_NOT_WRITE_NEW_MANIFEST=1 \
    "$zed" install zed-pkg/docker-node-lib@^1 \
      --registry "file://$registry" \
      --home "$home/env-ephemeral" \
      --install-mode copy
)
test ! -e "$env_ephemeral/.zpkg.toml"
test -f "$env_ephemeral/.zpkg.lock"

# Legacy spellings remain functional during the compatibility window and emit
# actionable migration guidance.
legacy="$root/legacy"
copy_consumer "$legacy"
(
  cd "$legacy"
  "$zed" install zed-pkg/docker-node-lib@^1 \
    --skip-manifest \
    --registry "file://$registry" \
    --home "$home/legacy" \
    --install-mode copy
) >"$root/legacy.log" 2>&1
test ! -e "$legacy/.zpkg.toml"
test -f "$legacy/.zpkg.lock"
grep -F -- '--skip-manifest is deprecated' "$root/legacy.log"

# A lock-only restore cannot be converted into a truthful direct-dependency
# manifest. It fails by default and succeeds only when ephemeral intent is
# explicit.
restore="$root/restore"
copy_consumer "$restore"
cp "$ephemeral/.zpkg.lock" "$restore/.zpkg.lock"
if (
  cd "$restore"
  "$zed" install --frozen \
    --registry "file://$registry" \
    --home "$home/ephemeral" \
    --install-mode copy
) >"$root/implicit-restore.log" 2>&1; then
  echo "implicit lock-only restore unexpectedly succeeded" >&2
  exit 1
fi
test ! -e "$restore/.zpkg.toml"
grep -F 'lockfile cannot identify which packages were direct dependencies' \
  "$root/implicit-restore.log"
(
  cd "$restore"
  "$zed" install --frozen --do-not-write-new-manifest \
    --registry "file://$registry" \
    --home "$home/ephemeral" \
    --install-mode copy
)
test ! -e "$restore/.zpkg.toml"
cmp "$ephemeral/.zpkg.lock" "$restore/.zpkg.lock"
test -d "$restore/zed_modules/zed-pkg/docker-node-lib"

printf 'durable first-install E2E passed: %s\n' "$root"
