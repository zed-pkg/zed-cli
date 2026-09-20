#!/bin/sh
# Install zed's Git hooks into a repository.
#   install-git-hooks.sh [repo-dir]        # default: current repo
# Strategy (least surprising first):
#   1. if core.hooksPath is already configured, honor it and install there;
#   2. otherwise, if the repo has .githooks/, opt into that tracked convention;
#   3. otherwise install into Git's default hooks directory.
# Existing hook files are chained as <name>.pre-zed so nothing the user had is lost.
set -eu
src=$(cd "$(dirname "$0")/../hooks" && pwd)
repo=${1:-.}
cd "$repo"
top=$(git rev-parse --show-toplevel)
gitdir=$(git rev-parse --git-dir)
cd "$top"
configured_hooks_path=$(git config --get core.hooksPath 2>/dev/null || true)
if [ -n "$configured_hooks_path" ]; then
  # `git rev-parse --git-path hooks` resolves core.hooksPath according to Git's
  # own semantics, including relative and absolute custom paths.
  dest=$(git rev-parse --git-path hooks)
  mkdir -p "$dest"
  mode=configured
elif [ -d .githooks ]; then
  dest=.githooks
  git config core.hooksPath .githooks
  mode=githooks
else
  dest="$gitdir/hooks"
  mkdir -p "$dest"
  mode=gitdir
fi
cp "$src/zed-git-hook.sh" "$dest/zed-git-hook.sh"; chmod +x "$dest/zed-git-hook.sh"
for h in post-checkout post-merge post-rewrite; do
  if [ -f "$dest/$h" ] && ! grep -q 'zed-git-hook.sh' "$dest/$h"; then
    mv "$dest/$h" "$dest/$h.pre-zed"
    { cat "$src/$h"; echo; echo '# chained pre-existing hook'; echo "[ -x \"\$here/$h.pre-zed\" ] && \"\$here/$h.pre-zed\" \"\$@\""; } > "$dest/$h"
  else
    cp "$src/$h" "$dest/$h"
  fi
  chmod +x "$dest/$h"
done
echo "[install-git-hooks] installed post-checkout/post-merge/post-rewrite into $dest ($mode) for $top"
