#!/usr/bin/env bash
# Runs the full zed-cli test suite inside a clean OCI container, proving the
# store/symlink/copy install paths behave identically under containerization
# (fresh user, fresh $HOME, no host toolchain). Requires docker or podman.
set -euo pipefail

runtime="${CONTAINER_RUNTIME:-}"
if [ -z "$runtime" ]; then
  if command -v docker >/dev/null 2>&1; then runtime=docker
  elif command -v podman >/dev/null 2>&1; then runtime=podman
  else
    echo "container-smoke: docker/podman not found; skipping" >&2
    exit 0
  fi
fi

# Parent dir must hold zed-cli and zed-interfaces side by side (path dep).
parent="$(cd "$(dirname "$0")/../.." && pwd)"

exec "$runtime" run --rm \
  -v "$parent":/work \
  -w /work/zed-cli \
  -e CARGO_TERM_COLOR=never \
  rust:1-slim \
  sh -c "cargo test --quiet && echo 'container smoke: OK'"
