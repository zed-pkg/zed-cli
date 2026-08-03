from __future__ import annotations

import contextlib
import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

import nix_to_zed as bridge

REVISION = "0123456789abcdef0123456789abcdef01234567"
NAR_HASH = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
LOCK = {
    "nodes": {
        "nixpkgs": {
            "locked": {
                "lastModified": 1782467914,
                "narHash": "sha256-pGvFkM8N0xEkIIXDe5YYfbEAvHrk4IxBrjB/x8OomhE=",
                "owner": "NixOS",
                "repo": "nixpkgs",
                "rev": "e73de5be04e0eff4190a1432b946d469c794e7b4",
                "type": "github",
            },
            "original": {
                "owner": "NixOS",
                "repo": "nixpkgs",
                "rev": "e73de5be04e0eff4190a1432b946d469c794e7b4",
                "type": "github",
            },
        },
        "root": {"inputs": {"nixpkgs": "nixpkgs"}},
    },
    "root": "root",
    "version": 7,
}


class NixToZedTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.output = self.root / "realized-output"
        (self.output / "bin").mkdir(parents=True)
        executable = self.output / "bin/portable"
        executable.write_text(
            "#!/bin/sh\nprintf 'portable\\n'\n", encoding="utf-8"
        )
        executable.chmod(0o755)
        (self.output / "share").mkdir()
        (self.output / "share/message.txt").write_text(
            "sealed bytes\n", encoding="utf-8"
        )

        self.path_info = self.root / "path-info.json"
        self.write_path_info()
        self.derivation = self.root / "derivation.json"
        self.derivation.write_text(
            json.dumps({"drv": {"outputs": {"out": {}}}}, sort_keys=True),
            encoding="utf-8",
        )
        self.flake_lock = self.root / "flake.lock"
        self.flake_lock.write_text(
            json.dumps(LOCK, sort_keys=True), encoding="utf-8"
        )

    def tearDown(self) -> None:
        self.temp.cleanup()

    def write_path_info(self, **overrides: object) -> None:
        value: dict[str, object] = {
            "path": str(self.output.resolve()),
            "narHash": NAR_HASH,
            "narSize": 123,
            "references": [],
            "signatures": [],
        }
        value.update(overrides)
        self.path_info.write_text(
            json.dumps([value], sort_keys=True), encoding="utf-8"
        )

    def invoke(self, *arguments: str) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = bridge.main(list(arguments))
        return status, stdout.getvalue(), stderr.getvalue()

    def seal(self, destination: Path, *extra: str) -> tuple[int, str, str]:
        return self.invoke(
            "seal",
            "--store-path",
            str(self.output),
            "--path-info",
            str(self.path_info),
            "--derivation-json",
            str(self.derivation),
            "--flake-lock",
            str(self.flake_lock),
            "--locked-ref",
            f"github:acme/portable/{REVISION}",
            "--attribute",
            "packages.x86_64-linux.portable",
            "--system",
            "x86_64-linux",
            "--output",
            "out",
            "--as-package",
            "acme/nix-portable@1.0.0",
            "--bin",
            "portable=bin/portable",
            "--repository",
            "https://github.com/acme/portable",
            "--source-revision",
            REVISION,
            "--license",
            "MIT",
            "--description",
            "Closure-free portable fixture",
            "--nix-version",
            "nix (Nix) 2.31.0",
            "--out-dir",
            str(destination),
            "--allow-local-store",
            *extra,
        )

    def verify(self, destination: Path) -> tuple[int, str, str]:
        return self.invoke(
            "verify",
            "--directory",
            str(destination),
            "--allow-local-store",
        )

    def test_seal_is_deterministic_canonical_and_runtime_independent(self) -> None:
        first = self.root / "first"
        second = self.root / "second"
        status, stdout, stderr = self.seal(first)
        self.assertEqual(status, 0, stderr)
        self.assertEqual(self.seal(second)[0], 0)
        artifact = first / "nix-portable-1.0.0.tar.gz"
        self.assertEqual(
            artifact.read_bytes(), (second / artifact.name).read_bytes()
        )
        self.assertEqual(
            (first / "zed-nix-adapter.json").read_bytes(),
            (second / "zed-nix-adapter.json").read_bytes(),
        )

        bridge_record = json.loads(stdout)
        self.assertEqual(bridge_record["schema"], bridge.BRIDGE_SCHEMA)
        adapter = json.loads((first / "zed-nix-adapter.json").read_text())
        self.assertEqual(adapter["direction"], "nix-to-zed")
        self.assertEqual(adapter["schema"], bridge.ADAPTER_SCHEMA)
        self.assertEqual(adapter["source"]["realized"]["references"], [])
        self.assertEqual(adapter["policy"], bridge.strict_policy())
        self.assertEqual(
            adapter["artifact"]["sha256"],
            hashlib.sha256(artifact.read_bytes()).hexdigest(),
        )

        status, _, stderr = self.verify(first)
        self.assertEqual(status, 0, stderr)
        with tarfile.open(artifact, "r:gz") as archive:
            names = set(archive.getnames())
            self.assertIn(".zpkg.toml", names)
            self.assertIn("zed-nix-runtime.json", names)
            runtime = json.load(archive.extractfile("zed-nix-runtime.json"))
            self.assertFalse(runtime["policy"]["nix_required_at_zed_runtime"])
            self.assertNotIn("store_path", runtime["origin"])
            self.assertEqual(
                runtime["origin"]["store_path_sha256"],
                hashlib.sha256(str(self.output.resolve()).encode()).hexdigest(),
            )
            manifest = archive.extractfile(".zpkg.toml").read().decode()
            self.assertIn('"portable" = "bin/portable"', manifest)

    def test_external_reference_fails_closed(self) -> None:
        self.write_path_info(
            references=["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc"]
        )
        status, _, stderr = self.seal(self.root / "references")
        self.assertEqual(status, 2)
        self.assertIn("runtime references", stderr)

    def test_runtime_store_string_and_unsafe_symlink_fail_closed(self) -> None:
        store_file = self.output / "share/store.txt"
        store_file.write_text(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc\n",
            encoding="utf-8",
        )
        status, _, stderr = self.seal(self.root / "store-string")
        self.assertEqual(status, 2)
        self.assertIn("runtime store reference", stderr)
        store_file.unlink()

        (self.output / "escape").symlink_to("../../outside")
        status, _, stderr = self.seal(self.root / "symlink")
        self.assertEqual(status, 2)
        self.assertIn("symlink escapes", stderr)

    def test_mutable_ref_revision_mismatch_and_missing_binary_fail_closed(self) -> None:
        status, _, stderr = self.seal(
            self.root / "mutable", "--locked-ref", "github:acme/portable/main"
        )
        self.assertEqual(status, 2)
        self.assertIn("immutable", stderr)

        other = "f" * 40
        status, _, stderr = self.seal(
            self.root / "mismatch", "--source-revision", other
        )
        self.assertEqual(status, 2)
        self.assertIn("not present", stderr)

        status, _, stderr = self.seal(
            self.root / "missing", "--bin", "missing=bin/missing"
        )
        self.assertEqual(status, 2)
        self.assertIn("does not resolve", stderr)

    def test_overwrite_symlink_and_control_file_collisions_fail_closed(self) -> None:
        destination = self.root / "existing"
        self.assertEqual(self.seal(destination)[0], 0)
        status, _, stderr = self.seal(destination)
        self.assertEqual(status, 2)
        self.assertIn("--force", stderr)
        self.assertEqual(self.seal(destination, "--force")[0], 0)

        victim = self.root / "victim"
        victim.write_text("keep", encoding="utf-8")
        symlink_destination = self.root / "symlink-output"
        symlink_destination.mkdir()
        (symlink_destination / "bridge.json").symlink_to(victim)
        status, _, stderr = self.seal(symlink_destination, "--force")
        self.assertEqual(status, 2)
        self.assertIn("symlink output", stderr)
        self.assertEqual(victim.read_text(), "keep")

        collision = self.output / ".zpkg.toml"
        collision.write_text("collision", encoding="utf-8")
        status, _, stderr = self.seal(self.root / "collision")
        self.assertEqual(status, 2)
        self.assertIn("reserved package path", stderr)

    def test_artifact_adapter_and_projection_tampering_are_detected(self) -> None:
        destination = self.root / "tamper"
        self.assertEqual(self.seal(destination)[0], 0)
        artifact = destination / "nix-portable-1.0.0.tar.gz"
        artifact.write_bytes(artifact.read_bytes() + b"tamper")
        status, _, stderr = self.verify(destination)
        self.assertEqual(status, 2)
        self.assertIn("artifact SHA-256 mismatch", stderr)

        destination = self.root / "adapter-tamper"
        self.assertEqual(self.seal(destination)[0], 0)
        adapter_path = destination / "zed-nix-adapter.json"
        adapter = json.loads(adapter_path.read_text())
        adapter["policy"]["builder_network"] = "allowed"
        adapter_path.write_text(
            json.dumps(adapter, sort_keys=True), encoding="utf-8"
        )
        status, _, stderr = self.verify(destination)
        self.assertEqual(status, 2)
        self.assertIn("adapter SHA-256 mismatch", stderr)

    def test_noncanonical_store_path_requires_explicit_fixture_flag(self) -> None:
        destination = self.root / "local"
        self.assertEqual(self.seal(destination)[0], 0)
        status, _, stderr = self.invoke(
            "verify", "--directory", str(destination)
        )
        self.assertEqual(status, 2)
        self.assertIn("invalid Nix store path", stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
