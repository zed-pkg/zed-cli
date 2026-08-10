from __future__ import annotations

import importlib.util
import json
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


def manifest(root: Path, name: str, version: str = "1.2.3", dependencies: dict[str, str] | None = None, *, role: str | None = None, family: str | None = None) -> None:
    directory = root / name
    directory.mkdir()
    lines = ["[package]", 'org = "acme"', f'name = "{name}"', f'version = "{version}"']
    if role:
        lines.append(f'role = "{role}"')
    if family:
        lines.append(f'family = "{family}"')
    if dependencies is not None:
        lines.extend(["", "[dependencies]"])
        for key, value in sorted(dependencies.items()):
            lines.append(f'"{key}" = "{value}"')
    (directory / ".zpkg.toml").write_text("\n".join(lines) + "\n", encoding="utf-8")


class RoleContractTests(unittest.TestCase):
    def test_complete_cli_family_passes(self) -> None:
        with tempfile.TemporaryDirectory() as value:
            root = Path(value)
            manifest(root, "demo-interfaces")
            manifest(root, "demo-lib", dependencies={"acme/demo-interfaces": "^1.2.3"})
            manifest(root, "demo-clients", dependencies={"acme/demo-interfaces": "^1.2.3"})
            manifest(root, "demo-cli", dependencies={
                "acme/demo-clients": "^1.2.3",
                "acme/demo-interfaces": "^1.2.3",
                "acme/demo-lib": "^1.2.3",
            })
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

            other = Path(value) / "explicit"
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


if __name__ == "__main__":
    unittest.main()
