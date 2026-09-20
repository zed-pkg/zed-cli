#!/bin/sh
set -eu

repo_root=$(git rev-parse --show-toplevel)
tmp_root=$(mktemp -d "${TMPDIR:-/tmp}/zed-hook-install.XXXXXX")
trap 'rm -rf "$tmp_root"' EXIT HUP INT TERM

new_repo() {
  name=$1
  dir="$tmp_root/$name"
  mkdir -p "$dir"
  git -C "$dir" init -q
  printf '%s\n' "$dir"
}

# Explicit installer must honor an existing custom hooks path rather than
# replacing it with the repository's tracked .githooks convention.
custom_repo=$(new_repo custom)
mkdir -p "$custom_repo/.githooks" "$custom_repo/custom-hooks"
git -C "$custom_repo" config core.hooksPath custom-hooks
"$repo_root/scripts/install-git-hooks.sh" "$custom_repo"
[ "$(git -C "$custom_repo" config --get core.hooksPath)" = custom-hooks ]
for hook in post-checkout post-merge post-rewrite; do
  [ -x "$custom_repo/custom-hooks/$hook" ] || {
    echo "expected installer to honor custom hooks path for $hook" >&2
    exit 1
  }
done

# With no pre-existing hook ownership, the tracked .githooks convention is
# activated deterministically.
tracked_repo=$(new_repo tracked)
mkdir -p "$tracked_repo/.githooks"
"$repo_root/scripts/install-git-hooks.sh" "$tracked_repo"
[ "$(git -C "$tracked_repo" config --get core.hooksPath)" = .githooks ]

# The tracked pre-push helper's convenience installer follows the same rule:
# activate only an unclaimed hooks path, never replace a custom one.
prepush_repo=$(new_repo prepush)
mkdir -p "$prepush_repo/.githooks" "$prepush_repo/company-hooks"
cp "$repo_root/.githooks/pre-push" "$prepush_repo/.githooks/pre-push"
cp "$repo_root/.githooks/pre-commit" "$prepush_repo/.githooks/pre-commit"
git -C "$prepush_repo" config core.hooksPath company-hooks
(
  cd "$prepush_repo"
  bash .githooks/pre-push --install
)
[ "$(git -C "$prepush_repo" config --get core.hooksPath)" = company-hooks ] || {
  echo 'pre-push --install replaced custom core.hooksPath' >&2
  exit 1
}

printf '%s\n' '[git-hook-install] PASS'
