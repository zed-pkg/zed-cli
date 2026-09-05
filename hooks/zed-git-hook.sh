#!/bin/sh
# Shared body for zed's Git hooks (post-checkout, post-merge, post-rewrite).
# When a checkout/merge/rewrite changes .gitmodules, .zpkg.toml or .zpkg.lock,
# run `zed install --git-submodules` so submodules and zed packages are
# materialized together. Never rebases, stashes, resets or pushes.
#
# Env:
#   ZED_SKIP_GIT_HOOK=1   skip entirely
#   ZED_GIT_HOOK_VERBOSE=1 print every decision
#   ZED_BIN               path to zed (default: first `zed` on PATH)
set -u
[ "${ZED_SKIP_GIT_HOOK:-0}" = "1" ] && exit 0
hook="${1:-unknown}"; old="${2:-}"; new="${3:-}"
log() { [ "${ZED_GIT_HOOK_VERBOSE:-0}" = "1" ] && echo "[zed-git-hook:$hook] $*" >&2; return 0; }

root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
cd "$root" || exit 0

# Only act inside a zed project or a submodule superproject.
if [ ! -f .zpkg.toml ] && [ ! -f .gitmodules ]; then log "no .zpkg.toml/.gitmodules; skip"; exit 0; fi

changed=1
if [ -n "$old" ] && [ -n "$new" ] && [ "$old" != "$new" ] && git cat-file -e "$old" 2>/dev/null; then
  if git diff --quiet "$old" "$new" -- .gitmodules .zpkg.toml .zpkg.lock 2>/dev/null; then changed=0; fi
fi
if [ "$changed" = "0" ]; then log "no dependency metadata changed between $old and $new; skip"; exit 0; fi

zed_bin="${ZED_BIN:-$(command -v zed 2>/dev/null || true)}"
if [ -z "$zed_bin" ]; then
  echo "[zed-git-hook:$hook] zed not found on PATH; run 'zed install --git-submodules' manually (or: git submodule update --init --recursive)" >&2
  exit 0
fi
frozen=""; [ -f .zpkg.lock ] && frozen="--frozen"
echo "[zed-git-hook:$hook] dependency metadata changed; running: zed install --git-submodules $frozen" >&2
# A hook must not block the git operation that already succeeded: report, don't fail.
"$zed_bin" install --git-submodules $frozen || echo "[zed-git-hook:$hook] zed install failed (exit $?); working tree is unchanged by the hook" >&2
exit 0
