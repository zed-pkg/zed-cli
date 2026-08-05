#!/usr/bin/env python3
from pathlib import Path

bridge_path = Path("nix/zed-package.nix")
bridge = bridge_path.read_text(encoding="utf-8")
marker = '''        mkdir -p "$out/tree" "$out/metadata"\n'''
replacement = '''        # The durable project operation descriptor coordinates concurrent
        # mutations, but its PID, operation name, and timestamp diagnostics are
        # acquisition-specific project state, not package output. Exporting it
        # would make the recursive fixed output nondeterministic.
        operation_lock="$work/.zed/operation.lock"
        if [[ -e "$operation_lock" ]]; then
          if [[ ! -f "$operation_lock" || -L "$operation_lock" ]]; then
            echo "zed-pkg Nix bridge: operation lock must be a regular file" >&2
            exit 1
          fi
          rm "$operation_lock"
          rmdir "$work/.zed" 2>/dev/null || true
        fi

        mkdir -p "$out/tree" "$out/metadata"
'''
if bridge.count(marker) != 1:
    raise SystemExit("expected exactly one fixed-output copy boundary")
bridge_path.write_text(bridge.replace(marker, replacement), encoding="utf-8")

workflow_path = Path(".github/workflows/nix-interop.yml")
workflow = workflow_path.read_text(encoding="utf-8")
assertion = '''          test ! -e "$deps_a/tree/.vendor/.zed/zed-pkg/docker-node-lib/generated/output.txt"\n'''
assertion_replacement = assertion + '''          test ! -e "$deps_a/tree/.zed/operation.lock"\n'''
if workflow.count(assertion) != 1:
    raise SystemExit("expected exactly one fixed-output assertion block")
workflow_path.write_text(
    workflow.replace(assertion, assertion_replacement),
    encoding="utf-8",
)
