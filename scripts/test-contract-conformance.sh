#!/bin/sh
set -eu

repo_root=$(cd "$(dirname "$0")/.." && pwd)
checker="$repo_root/conformance/check.sh"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/zed-conformance-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

new_case() {
  name=$1
  dir="$tmp/$name"
  mkdir -p "$dir/contracts" "$dir/conformance"
  cp "$checker" "$dir/conformance/check.sh"
  printf '%s\n' "$dir"
}

ok=$(new_case ok)
(
  cd "$ok"
  sh conformance/check.sh --structural-only >/dev/null
)

missing=$(new_case missing)
rm -rf "$missing/conformance"
if (cd "$missing" && sh "$checker" --structural-only >/dev/null 2>&1); then
  echo "missing conformance/ unexpectedly passed" >&2
  exit 1
fi

symlink=$(new_case symlink)
rm -rf "$symlink/contracts"
mkdir "$symlink/real-contracts"
ln -s real-contracts "$symlink/contracts"
if (cd "$symlink" && sh conformance/check.sh --structural-only >/dev/null 2>&1); then
  echo "symlinked contracts/ unexpectedly passed" >&2
  exit 1
fi

delegate=$(new_case delegate)
cat >"$delegate/conformance/run.sh" <<'EOF'
#!/bin/sh
set -eu
: "${ZED_CONFORMANCE_ROOT:?}"
: "${ZED_CONTRACT_ROOT:?}"
printf passed > conformance/delegated.marker
EOF
(
  cd "$delegate"
  sh conformance/check.sh --full >/dev/null
  test "$(cat conformance/delegated.marker)" = passed
)

nested=$(new_case nested-symlink)
mkdir -p "$nested/contracts/schema"
ln -s ../../conformance "$nested/contracts/schema/escape"
if (cd "$nested" && sh conformance/check.sh --structural-only >/dev/null 2>&1); then
  echo "nested boundary symlink unexpectedly passed" >&2
  exit 1
fi

echo "contract/conformance lifecycle gate tests passed"
