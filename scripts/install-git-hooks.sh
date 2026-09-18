#!/bin/sh
# Install zed's Git hooks into a repository.
#   install-git-hooks.sh [repo-dir]        # default: current repo
# Strategy (least surprising first):
#   1. repo has .githooks/ (k8s-cluster convention) → copy hooks there and
#      ensure `git config core.hooksPath .githooks`
#   2. otherwise copy into .git/hooks/, chaining any pre-existing hook as
#      <name>.pre-zed so nothing the user had is lost.
set -eu
src=$(cd "$(dirname "$0")/../hooks" && pwd)
repo=${1:-.}
cd "$repo"
top=$(git rev-parse --show-toplevel)
gitdir=$(git rev-parse --git-dir)
cd "$top"
if [ -d .githooks ]; then dest=.githooks; git config core.hooksPath .githooks; mode=githooks
else dest="$gitdir/hooks"; mkdir -p "$dest"; mode=gitdir; fi
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
