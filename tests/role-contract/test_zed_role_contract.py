from __future__ import annotations

import importlib.util
import json
import shlex
import sys
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).parents[2] / "tools" / "role-contract" / "zed_role_contract.py"
spec = importlib.util.spec_from_file_location("zed_role_contract", MODULE_PATH)
assert spec and spec.loader
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)


def manifest(root: Path, name: str, version: str = "1.2.3", dependencies: dict[str, str] | None = None, *, role: str | None = None, family: str | None = None, extra: str = "") -> Path:
    directory = root / name
    directory.mkdir()
    lines = ["# preserve-top-comment", "[package]", 'org = "acme"', f'name = "{name}"', f'version = "{version}"']
    if role:
        lines.append(f'role = "{role}"')
    if family:
        lines.append(f'family = "{family}"')
    if extra:
        lines.extend(["", extra.rstrip("\n")])
    if dependencies is not None:
        lines.extend(["", "[dependencies]"])
        for key, value in sorted(dependencies.items()):
            lines.append(f'"{key}" = "{value}"')
    path = directory / ".zpkg.toml"
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def lock_command(root: Path, *, fail: bool = False) -> tuple[str, Path]:
    script = root / ("fail_lock.py" if fail else "refresh_lock.py")
    counter = root / "lock-count.txt"
    script.write_text(("raise SystemExit(9)\n" if fail else "from pathlib import Path\n" "root = Path.cwd()\n" f"counter = Path({str(counter)!r})\n" "value = int(counter.read_text() or '0') if counter.exists() else 0\n" "counter.write_text(str(value + 1), encoding='utf-8')\n" "(root / '.zpkg.lock').write_text('schema = 1\\n', encoding='utf-8')\n"), encoding="utf-8")
    return f"{shlex.quote(sys.executable)} {shlex.quote(str(script))}", counter


class RoleContractTests(unittest.TestCase):
    def test_complete_cli_family_passes(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "demo-interfaces")
            manifest(root, "demo-lib", dependencies={"acme/demo-interfaces": "^1.2.3"})
            manifest(root, "demo-clients", dependencies={"acme/demo-interfaces": "^1.2.3"})
            manifest(root, "demo-cli", dependencies={"acme/demo-clients": "^1.2.3", "acme/demo-interfaces": "^1.2.3", "acme/demo-lib": "^1.2.3"})
            self.assertEqual(module.main(["check", str(root), "--output", str(root / "out.json")]), 0)

    def test_missing_producer_is_explicit_blocker(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "demo-cli", dependencies={})
            output = root / "audit.json"
            self.assertEqual(module.main(["audit", str(root), "--output", str(output)]), 0)
            report = json.loads(output.read_text())
            self.assertIn("PRODUCER_MISSING", {item["code"] for item in report["findings"]})
            self.assertEqual(module.main(["check", str(root), "--output", str(output)]), 2)

    def test_ambiguous_name_requires_explicit_metadata(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "toolbox", dependencies={})
            output = root / "audit.json"
            module.main(["audit", str(root), "--output", str(output)])
            report = json.loads(output.read_text())
            self.assertIn("ROLE_AMBIGUOUS", {item["code"] for item in report["findings"]})
            other = root / "explicit"
            other.mkdir()
            manifest(other, "toolbox", dependencies={}, role="interfaces", family="demo")
            self.assertEqual(module.main(["check", str(other), "--output", str(other / "out.json")]), 0)

    def test_stale_constraint_comes_from_producer_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "demo-interfaces", version="4.5.6")
            manifest(root, "demo-clients", dependencies={"acme/demo-interfaces": "^1.0.0"})
            output = root / "audit.json"
            module.main(["audit", str(root), "--output", str(output)])
            report = json.loads(output.read_text())
            finding = next(item for item in report["findings"] if item["code"] == "DEPENDENCY_CONSTRAINT_STALE")
            self.assertIn("^4.5.6", finding["message"])

    def test_fix_preserves_comments_unknown_tables_and_refreshes_lock(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "demo-interfaces", version="4.5.6")
            client = manifest(root, "demo-clients", dependencies=None, extra='[targets.rust]\ndir = "rust" # preserve-inline')
            command, counter = lock_command(root)
            output = root / "fix.json"
            self.assertEqual(module.main(["fix", str(root), "--lock-command", command, "--output", str(output)]), 0)
            updated = client.read_text(encoding="utf-8")
            self.assertIn("# preserve-top-comment", updated)
            self.assertIn('dir = "rust" # preserve-inline', updated)
            self.assertIn('"acme/demo-interfaces" = "^4.5.6"', updated)
            self.assertEqual(counter.read_text(encoding="utf-8"), "1")
            self.assertTrue((client.parent / ".zpkg.lock").is_file())
            report = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(report["fix"]["status"], "applied")
            self.assertTrue(report["ok"])

    def test_fix_replaces_only_stale_value_and_is_idempotent(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "demo-interfaces", version="4.5.6")
            client = manifest(root, "demo-clients", dependencies={"acme/demo-interfaces": "^1.0.0"})
            client.write_text(client.read_text().replace('"acme/demo-interfaces" = "^1.0.0"', '"acme/demo-interfaces" = "^1.0.0" # keep-this'), encoding="utf-8")
            command, counter = lock_command(root)
            args = ["fix", str(root), "--lock-command", command, "--output", str(root / "fix.json")]
            self.assertEqual(module.main(args), 0)
            self.assertIn('"acme/demo-interfaces" = "^4.5.6" # keep-this', client.read_text())
            self.assertEqual(module.main(args), 0)
            self.assertEqual(counter.read_text(), "1")
            self.assertEqual(json.loads((root / "fix.json").read_text())["fix"]["status"], "clean")

    def test_dry_run_writes_nothing_and_runs_no_lock_command(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "demo-interfaces")
            client = manifest(root, "demo-clients", dependencies={})
            before = client.read_bytes()
            command, counter = lock_command(root)
            output = root / "fix.json"
            self.assertEqual(module.main(["fix", str(root), "--dry-run", "--lock-command", command, "--output", str(output)]), 0)
            self.assertEqual(client.read_bytes(), before)
            self.assertFalse(counter.exists())
            self.assertFalse((client.parent / ".zpkg.lock").exists())
            self.assertEqual(json.loads(output.read_text())["fix"]["status"], "planned")

    def test_lock_failure_rolls_manifest_and_lock_back(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "demo-interfaces")
            client = manifest(root, "demo-clients", dependencies={})
            lock = client.parent / ".zpkg.lock"
            lock.write_text("original-lock\n")
            before_manifest, before_lock = client.read_bytes(), lock.read_bytes()
            command, _ = lock_command(root, fail=True)
            output = root / "fix.json"
            self.assertEqual(module.main(["fix", str(root), "--lock-command", command, "--output", str(output)]), 2)
            self.assertEqual(client.read_bytes(), before_manifest)
            self.assertEqual(lock.read_bytes(), before_lock)
            self.assertEqual(json.loads(output.read_text())["fix"]["status"], "rolled-back")

    def test_noneditable_findings_block_without_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            client = manifest(root, "demo-clients", dependencies={})
            before = client.read_bytes()
            command, counter = lock_command(root)
            output = root / "fix.json"
            self.assertEqual(module.main(["fix", str(root), "--lock-command", command, "--output", str(output)]), 2)
            self.assertEqual(client.read_bytes(), before)
            self.assertFalse(counter.exists())
            self.assertEqual(json.loads(output.read_text())["fix"]["status"], "blocked")


if __name__ == "__main__":
    unittest.main()
