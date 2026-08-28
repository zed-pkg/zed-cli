#!/usr/bin/env bash
# Freeze generated artifacts. Git does not store the Unix write bit, so clones
# come back writable; run this after checkout or let `f2e generate` / `ridl generate`
# chmod for you.
set -euo pipefail
root="${1:-.}"
if [[ ! -d "$root" ]]; then
  echo "freeze-generated: not a directory: $root" >&2
  exit 1
fi
# README.md stays writable so the policy doc can be updated.
find "$root" \( \
  -path '*/node_modules/*' -o \
  -path '*/target/*' -o \
  -path '*/.git/*' -o \
  -path '*/.dart_tool/*' \
\) -prune -o \
  -type d -name generated -print | while IFS= read -r dir; do
  if [[ ! -f "$dir/README.md" ]]; then
    continue
  fi
  if grep -q 'not frozen' "$dir/README.md" 2>/dev/null; then
    continue
  fi
  if ! grep -qi 'frozen' "$dir/README.md" 2>/dev/null; then
    continue
  fi
  find "$dir" -type f ! -name 'README.md' ! -name '.gitkeep' -print0 |
    xargs -0 chmod a-w
  echo "froze $dir"
done
