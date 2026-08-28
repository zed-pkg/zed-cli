#!/usr/bin/env bash
set -euo pipefail

repository_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
loader="$repository_root/scripts/ci/r2_load_protected_credentials.sh"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

run_loader() {
  local ref=$1
  shift
  env \
    RUNNER_TEMP="$scratch" \
    GITHUB_RUN_ID=12345 \
    GITHUB_RUN_ATTEMPT=2 \
    GITHUB_REPOSITORY=zed-pkg/zed-cli \
    GITHUB_REF="$ref" \
    GITHUB_WORKFLOW='R2 test' \
    GITHUB_JOB=real-r2-roundtrip \
    GITHUB_SHA=0123456789abcdef0123456789abcdef01234567 \
    GITHUB_ENV="$scratch/github-env" \
    GITHUB_EVENT_NAME=workflow_dispatch \
    SECRET_ACCOUNT_ID=0123456789abcdef0123456789abcdef \
    SECRET_API_TOKEN=aaaaaaaaaaaaaaaaaaaa \
    SECRET_PARENT_ACCESS_KEY_ID=bbbbbbbbbbbbbbbb \
    bash "$loader" "$@"
}

run_loader refs/heads/main >"$scratch/output"
credential_file="$scratch/r2-parent-delegation.json"
test -f "$credential_file"
test "$(stat -c '%a' "$credential_file")" = 600
jq -e '
  .run == "12345"
  and .attempt == "2"
  and .repository == "zed-pkg/zed-cli"
  and .ref == "refs/heads/main"
  and .sha == "0123456789abcdef0123456789abcdef01234567"
' "$credential_file" >/dev/null
grep -Fx 'R2_CREDENTIAL_SOURCE=protected-actions-environment' "$scratch/github-env" >/dev/null
rm -f "$credential_file" "$scratch/github-env"

if run_loader refs/heads/feature >/dev/null 2>&1; then
  echo 'credential loader accepted a non-main ref' >&2
  exit 1
fi

if run_loader refs/heads/main unexpected >/dev/null 2>&1; then
  echo 'credential loader accepted an unexpected argument' >&2
  exit 1
fi

ln -s /dev/null "$credential_file"
if run_loader refs/heads/main >/dev/null 2>&1; then
  echo 'credential loader accepted a pre-existing symlink destination' >&2
  exit 1
fi
test -L "$credential_file"
