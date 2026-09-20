#!/bin/sh
# Shared body for zed's Git hooks (post-checkout, post-merge, post-rewrite).
# Dependency metadata changes trigger `zed install --git-submodules`; contract
# or conformance changes trigger the repository-owned conformance entry point.
# These post-operation hooks report failures but never undo/block a Git action
# that already succeeded. Never rebases, stashes, resets or pushes.
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

dependency_changed=1
boundary_changed=1
if [ -n "$old" ] && [ -n "$new" ] && [ "$old" != "$new" ] && git cat-file -e "$old" 2>/dev/null; then
  git diff --quiet "$old" "$new" -- .gitmodules .zpkg.toml .zpkg.lock 2>/dev/null && dependency_changed=0
  git diff --quiet "$old" "$new" -- contracts conformance 2>/dev/null && boundary_changed=0
fi
if [ "$dependency_changed" = "0" ] && [ "$boundary_changed" = "0" ]; then
  log "no dependency or contract/conformance metadata changed between $old and $new"
  exit 0
fi

zed_bin="${ZED_BIN:-$(command -v zed 2>/dev/null || true)}"
if [ "$dependency_changed" = "1" ]; then
  if [ -z "$zed_bin" ]; then
    echo "[zed-git-hook:$hook] zed not found on PATH; run 'zed install --git-submodules' manually (or: git submodule update --init --recursive)" >&2
  else
    frozen=""; [ -f .zpkg.lock ] && frozen="--frozen"
    echo "[zed-git-hook:$hook] dependency metadata changed; running: zed install --git-submodules $frozen" >&2
    "$zed_bin" install --git-submodules $frozen || echo "[zed-git-hook:$hook] zed install failed (exit $?); inspect the checkout before continuing" >&2
  fi
fi

# Contract/conformance checks are project-owned trusted code, just like the
# lifecycle hooks in .zpkg.toml. Post hooks report failure rather than masking
# the checkout/merge/rewrite that has already completed.
if [ "$boundary_changed" = "1" ] || [ "$dependency_changed" = "1" ]; then
  if [ -f conformance/check.sh ]; then
    sh conformance/check.sh --full || echo "[zed-git-hook:$hook] contract/conformance check failed (exit $?); inspect the checkout before continuing" >&2
  elif [ -e contracts ] || [ -e conformance ]; then
    echo "[zed-git-hook:$hook] contracts/conformance boundary exists but conformance/check.sh is missing" >&2
  fi
fi
exit 0
