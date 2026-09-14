#!/usr/bin/env python3
"""Fail-closed static admission for the DEN-2037 consumer lock evidence lane."""

from __future__ import annotations

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(f"DEN-2037 evidence failure: {message}")


manifest = json.loads(read("conformance/den-2037-local-lock-consumer.json"))
tasks = manifest.get("tasks", [])
require(len(tasks) == 15, f"expected exactly 15 tasks, got {len(tasks)}")
require([task.get("id") for task in tasks] == list(range(1, 16)), "task ids must be exactly 1..15")
require(len({task.get("name") for task in tasks}) == 15, "task names must be unique")

cargo = read("Cargo.toml")
match = re.search(
    r'zed-lock\s*=\s*\{\s*git\s*=\s*"https://github.com/zed-pkg/zed-lock\.git",\s*rev\s*=\s*"([0-9a-f]{40})"\s*\}',
    cargo,
)
require(match is not None, "Cargo.toml must pin zed-lock by exact 40-hex Git revision")
zed_lock_rev = match.group(1)

blocking_workflow = read(".github/workflows/blocking-store-process-locks.yml")
require(zed_lock_rev in blocking_workflow, "blocking-store-process-locks workflow must verify the exact Cargo zed-lock revision")

locking_docs = read("docs/locking.md")
require(zed_lock_rev in locking_docs, "docs/locking.md must name the exact production zed-lock revision")
require("Ordinary same-host installs" in locking_docs, "locking docs must keep Fiducia/distributed coordination out of ordinary same-host locking")
require("must never delete a lock file" in locking_docs, "locking docs must forbid stale lock-file deletion")

store = read("src/store.rs")
require("use zed_lock::{LockClass, LockManager, LockRequest};" in store, "Store must use the zed-lock authority")
require("acquire_blocking" in store, "Store local locks must use native blocking acquisition")
require("fiducia" not in store.lower(), "Store local lock implementation must not depend on Fiducia")
require("redis" not in store.lower(), "Store local lock implementation must not depend on Redis")
require("cloudflare" not in store.lower(), "Store local lock implementation must not depend on Cloudflare")

store_tests = read("tests/store_lock_evented.rs")
for test_name in (
    "contended_install_waiters_wake_and_serialize_after_release",
    "same_build_key_serializes_while_a_distinct_key_progresses",
    "abrupt_owner_exit_releases_the_kernel_lock_without_deleting_the_lock_file",
):
    require(test_name in store_tests, f"missing process-lock regression {test_name}")

consumer_tests = read("tests/den_2037_lock_consumer.rs")
for test_name in (
    "install_lock_scales_across_1_2_4_8_16_32_processes_without_overlap",
    "clean_home_bootstraps_lock_root_without_destroying_unrelated_content",
    "unknown_legacy_lock_artifacts_are_preserved",
):
    require(test_name in consumer_tests, f"missing DEN-2037 consumer regression {test_name}")
if "unix" in consumer_tests:
    require("symlinked_home_contends_with_real_home" in consumer_tests, "missing symlink-home alias regression")

frozen = read("tests/frozen_offline_prefetch.rs")
require("fs::remove_dir_all(&registry)" in frozen, "offline replay must delete the staged registry before replay")
require("from_store" in frozen and "from_cache" in frozen, "offline replay must prove both store and cache restoration")

offline_workflow = read(".github/workflows/frozen-offline-prefetch.yml")
for runner in ("ubuntu-24.04", "macos-15", "windows-2025"):
    require(runner in offline_workflow, f"offline replay must execute on {runner}")

transaction = read("src/transaction.rs")
require("Persist intent first" in transaction, "transaction recovery must persist intent before moving protected state")
require("interrupted_uuid_transaction_is_recovered_on_next_begin" in transaction, "missing interrupted transaction recovery regression")

namespace = json.loads(read("conformance/den-2037-lock-namespace.json"))
require(namespace.get("policy", {}).get("package_name_in_artifact_lock") is False, "artifact identity must not interpolate package names")
require(len(namespace.get("cases", [])) >= 5, "lock namespace corpus must include adversarial and granularity cases")

telemetry = read("docs/lock-telemetry.md").lower()
for forbidden in ("raw lock file paths", "artifact sha-256/build keys", "credentials"):
    require(forbidden in telemetry, f"telemetry contract must explicitly prohibit {forbidden}")

workflow = read(".github/workflows/den-2037-local-lock-consumer.yml")
for runner in ("ubuntu-24.04", "macos-15", "windows-2025"):
    require(runner in workflow, f"DEN-2037 consumer workflow must run on {runner}")
for command in (
    "check-den-2037-local-lock-consumer.py",
    "den_2037_lock_consumer",
    "store_lock_evented",
    "frozen_offline_prefetch",
    "transaction::tests::interrupted_uuid_transaction_is_recovered_on_next_begin",
    "0fc100afc3cd60b5ce091b4207f910bf08f2cfb7",
    "zed-lock-version-skew",
):
    require(command in workflow, f"DEN-2037 workflow is missing executable evidence command {command}")

pending = [task["id"] for task in tasks if task.get("evidence") == "audit_pending"]
print(json.dumps({
    "schema": manifest["schema"],
    "status": "admitted",
    "zed_lock_rev": zed_lock_rev,
    "tasks_total": len(tasks),
    "tasks_pending_implementation": pending,
}, sort_keys=True))
