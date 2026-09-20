#!/bin/sh
set -eu

mode=${1:-full}
case "$mode" in
  full|--full) mode=full ;;
  structural|--structural-only) mode=structural ;;
  *) echo "usage: conformance/check.sh [--full|--structural-only]" >&2; exit 2 ;;
esac

root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$root"

fail() {
  echo "[zed-conformance] $*" >&2
  exit 1
}

for boundary in contracts conformance; do
  [ ! -L "$boundary" ] || fail "$boundary must be a real directory, not a symbolic link"
  [ -d "$boundary" ] || fail "missing required top-level $boundary/ boundary"
done

escaped=$(find contracts conformance -type l -print -quit 2>/dev/null || true)
[ -z "$escaped" ] || fail "symbolic links are not allowed inside contract/conformance boundaries: $escaped"

contract_files=$(find contracts -type f ! -name README.md -print 2>/dev/null | wc -l | tr -d ' ')
conformance_files=$(find conformance -type f ! -name README.md ! -name check.sh -print 2>/dev/null | wc -l | tr -d ' ')
echo "[zed-conformance] boundaries ok: contracts=$contract_files conformance=$conformance_files"

[ "$mode" = full ] || exit 0
[ "${ZED_CONFORMANCE_DISPATCHED:-0}" != 1 ] || exit 0

runner=""
for candidate in conformance/run conformance/run.sh; do
  if [ -e "$candidate" ]; then
    [ ! -L "$candidate" ] || fail "$candidate must not be a symbolic link"
    [ -f "$candidate" ] || fail "$candidate must be a regular file"
    runner=$candidate
    break
  fi
done

if [ -z "$runner" ]; then
  echo "[zed-conformance] no behavioral runner declared; structural boundary check only"
  exit 0
fi

export ZED_CONFORMANCE_DISPATCHED=1
export ZED_CONFORMANCE_ROOT="$root/conformance"
export ZED_CONTRACT_ROOT="$root/contracts"
echo "[zed-conformance] running $runner"
case "$runner" in
  *.sh) exec sh "$runner" ;;
  *)
    [ -x "$runner" ] || fail "$runner must be executable or use the .sh suffix"
    exec "$runner"
    ;;
esac
