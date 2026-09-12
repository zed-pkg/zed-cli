from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

SCRIPT = (
    Path(__file__).resolve().parents[2]
    / "tools"
    / "role-contract"
    / "github_interface_fleet.py"
)
SPEC = importlib.util.spec_from_file_location("github_interface_fleet", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def zpkg(role: str, family: str, dependency: str | None = None) -> str:
    dependencies = ""
    if dependency:
        dependencies = f'\n[dependencies]\n"{dependency}" = "^1"\n'
    return (
        "[package]\n"
        f'org = "acme"\nname = "{family}-{role}"\nversion = "1.0.0"\n'
        f'role = "{role}"\nfamily = "{family}"\n'
        f"{dependencies}"
    )


class GitHubInterfaceFleetTests(unittest.TestCase):
    def repository(self, full_name: str, files: dict[str, str] | None = None, **kwargs):
        org, name = full_name.split("/", 1)
        return MODULE.Repository(
            org=org,
            name=name,
            full_name=full_name,
            files=files or {},
            **kwargs,
        )

    def test_complete_family_requires_roles_dependency_lock_and_source_import(self):
        coordinate = "acme/ledger-interfaces"
        repositories = [
            self.repository(
                coordinate,
                {
                    ".zpkg.toml": zpkg("interfaces", "ledger"),
                    ".zpkg.lock": "{}\n",
                },
            ),
            self.repository(
                "acme/ledger-web-server.rs",
                {
                    ".zpkg.toml": zpkg("server", "ledger", coordinate),
                    ".zpkg.lock": "{}\n",
                    "src/http.rs": "use ledger_interfaces::AccountView;\n",
                },
            ),
            self.repository(
                "acme/ledger-api-server.rs",
                {
                    ".zpkg.toml": zpkg("server", "ledger", coordinate),
                    ".zpkg.lock": "{}\n",
                    "src/routes.rs": "fn boundary(_: ledger_interfaces::WriteRequest) {}\n",
                },
            ),
        ]

        audits = MODULE.audit(repositories)
        self.assertEqual(len(audits), 1)
        self.assertTrue(audits[0].conformant)
        self.assertEqual(audits[0].web.repository, "acme/ledger-web-server.rs")
        self.assertEqual(audits[0].api.repository, "acme/ledger-api-server.rs")

    def test_metadata_only_dependencies_fail(self):
        coordinate = "acme/auth-interfaces"
        repositories = [
            self.repository(
                coordinate,
                {".zpkg.toml": zpkg("interfaces", "auth")},
            ),
            self.repository(
                "acme/auth-web-server.rs",
                {
                    ".zpkg.toml": zpkg("server", "auth", coordinate),
                    ".zpkg.lock": "{}\n",
                    "src/main.rs": "fn main() {}\n",
                },
            ),
            self.repository(
                "acme/auth-api-server.rs",
                {
                    ".zpkg.toml": zpkg("server", "auth", coordinate),
                    "src/main.rs": "use auth_interfaces as interfaces;\n",
                },
            ),
        ]

        audit = MODULE.audit(repositories)[0]
        self.assertFalse(audit.conformant)
        self.assertIn(
            "no source import of generated crate auth_interfaces",
            audit.web.findings,
        )
        self.assertIn("missing resolver-created .zpkg.lock", audit.api.findings)

    def test_missing_server_and_bad_producer_are_reported(self):
        repositories = [
            self.repository(
                "acme/chat-interfaces",
                {
                    ".zpkg.toml": zpkg("clients", "wrong"),
                },
            ),
            self.repository(
                "acme/chat-api-server.rs",
                {
                    ".zpkg.toml": zpkg(
                        "server", "chat", "acme/chat-interfaces"
                    ),
                    ".zpkg.lock": "{}\n",
                    "src/lib.rs": "use chat_interfaces as generated;\n",
                },
            ),
        ]

        audit = MODULE.audit(repositories)[0]
        self.assertIn("package.role must be interfaces", audit.producer_findings)
        self.assertIn("package.family must be chat", audit.producer_findings)
        self.assertIsNone(audit.web.repository)
        self.assertEqual(audit.web.findings, ("missing web server repository",))

    def test_archived_producers_are_ignored(self):
        repositories = [
            self.repository(
                "acme/old-interfaces",
                {".zpkg.toml": zpkg("interfaces", "old")},
                archived=True,
            )
        ]
        self.assertEqual(MODULE.audit(repositories), [])

    def test_snapshot_loader_is_deterministic(self):
        payload = [
            {
                "full_name": "acme/demo-interfaces",
                "files": {".zpkg.toml": zpkg("interfaces", "demo")},
            }
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "snapshot.json"
            path.write_text(json.dumps(payload), encoding="utf-8")
            loaded = MODULE.load_snapshot(path)
        self.assertEqual(loaded[0].org, "acme")
        self.assertEqual(loaded[0].name, "demo-interfaces")
        self.assertEqual(loaded[0].files[".zpkg.toml"], payload[0]["files"][".zpkg.toml"])


if __name__ == "__main__":
    unittest.main()
