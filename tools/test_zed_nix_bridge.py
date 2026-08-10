from __future__ import annotations

import contextlib
import hashlib
import io
import json
from pathlib import Path
import stat
import tarfile
import tempfile
import unittest

import zed_nix_bridge as bridge


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
MANIFEST = '''[package]
org = "acme"
name = "hello-data"
version = "1.2.3"
description = "Immutable hello fixture"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/hello-data"
'''


class BridgeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.manifest = self.root / ".zpkg.toml"
        self.manifest.write_text(MANIFEST, encoding="utf-8")
        self.artifact = self.root / "hello-data.tar.gz"
        self.artifact.write_bytes(b"deterministic-test-artifact")
        self.digest = hashlib.sha256(self.artifact.read_bytes()).hexdigest()
        self.metadata = self.root / "version.json"
        self.write_metadata()
        self.nixpkgs_lock = self.root / "flake.lock"
        self.nixpkgs_lock.write_text(json.dumps(LOCK), encoding="utf-8")

        self.output = self.root / "nix-output"
        (self.output / "bin").mkdir(parents=True)
        executable = self.output / "bin/portable"
        executable.write_text("#!/bin/sh\nprintf 'portable\\n'\n", encoding="utf-8")
        executable.chmod(0o755)
        (self.output / "share").mkdir()
        (self.output / "share/message.txt").write_text("sealed bytes\n", encoding="utf-8")
        self.path_info = self.root / "path-info.json"
        self.write_path_info()
        self.derivation = self.root / "derivation.json"
        self.derivation.write_text(json.dumps({"drv": {"outputs": {"out": {}}}}), encoding="utf-8")
        self.import_lock = self.root / "import-flake.lock"
        self.import_lock.write_text(json.dumps(LOCK), encoding="utf-8")

    def tearDown(self) -> None:
        self.temp.cleanup()

    def write_metadata(self, **overrides: object) -> None:
        value: dict[str, object] = {
            "org": "acme",
            "name": "hello-data",
            "version": "1.2.3",
            "sha256": self.digest,
            "format": "tar.gz",
            "download_url": self.artifact.as_uri(),
            "vcs_tag": "v1.2.3",
            "vcs_commit": REVISION,
        }
        value.update(overrides)
        self.metadata.write_text(json.dumps(value), encoding="utf-8")

    def write_path_info(self, **overrides: object) -> None:
        value: dict[str, object] = {
            "path": str(self.output.resolve()),
            "narHash": NAR_HASH,
            "narSize": 123,
            "references": [],
            "signatures": [],
        }
        value.update(overrides)
        self.path_info.write_text(json.dumps([value]), encoding="utf-8")

    def invoke(self, *arguments: str) -> tuple[int, str, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            status = bridge.main(list(arguments))
        return status, stdout.getvalue(), stderr.getvalue()

    def export(self, output: Path, *extra: str) -> tuple[int, str, str]:
        return self.invoke(
            "zed-to-nix",
            "--manifest",
            str(self.manifest),
            "--metadata",
            str(self.metadata),
            "--nixpkgs-lock",
            str(self.nixpkgs_lock),
            "--out-dir",
            str(output),
            "--allow-local-source",
            *extra,
        )

    def seal(self, output: Path, *extra: str) -> tuple[int, str, str]:
        return self.invoke(
            "nix-to-zed",
            "--store-path",
            str(self.output),
            "--path-info",
            str(self.path_info),
            "--derivation-json",
            str(self.derivation),
            "--flake-lock",
            str(self.import_lock),
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
            "--source-available",
            "--license",
            "MIT",
            "--description",
            "Closure-free portable Nix fixture",
            "--nix-version",
            "nix (Nix) 2.31.0",
            "--out-dir",
            str(output),
            "--allow-local-store",
            *extra,
        )

    def test_zed_export_is_deterministic_and_verifiable(self) -> None:
        first = self.root / "export-first"
        second = self.root / "export-second"
        self.assertEqual(self.export(first)[0], 0)
        self.assertEqual(self.export(second)[0], 0)
        for relative in [
            "README.md",
            "flake.lock",
            "flake.nix",
            "nix/package.nix",
            "zed-nix-adapter.json",
        ]:
            self.assertEqual((first / relative).read_bytes(), (second / relative).read_bytes())
        status, stdout, stderr = self.invoke("verify", "--directory", str(first))
        self.assertEqual(status, 0, stderr)
        self.assertEqual(json.loads(stdout)["direction"], "zed-to-nix")
        package_nix = (first / "nix/package.nix").read_text(encoding="utf-8")
        self.assertIn(bridge.hex_to_sri(self.digest), package_nix)
        self.assertIn("passthru.zed", package_nix)

    def test_export_rejects_identity_build_dependency_and_lock_drift(self) -> None:
        self.write_metadata(name="other")
        status, _, stderr = self.export(self.root / "bad-name")
        self.assertEqual(status, 2)
        self.assertIn("metadata name mismatch", stderr)
        self.write_metadata()
        self.manifest.write_text(MANIFEST + '\n[build]\ncommand = "curl bad | sh"\n', encoding="utf-8")
        status, _, stderr = self.export(self.root / "bad-build")
        self.assertEqual(status, 2)
        self.assertIn("refuses arbitrary [build]", stderr)
        self.manifest.write_text(MANIFEST + '\n[dependencies]\n"acme/dep" = "^1"\n', encoding="utf-8")
        status, _, stderr = self.export(self.root / "bad-deps")
        self.assertEqual(status, 2)
        self.assertIn("dependency-free", stderr)
        self.manifest.write_text(MANIFEST, encoding="utf-8")
        status, _, stderr = self.export(
            self.root / "bad-lock",
            "--nixpkgs-url",
            f"github:NixOS/nixpkgs/{REVISION}",
        )
        self.assertEqual(status, 2)
        self.assertIn("does not match", stderr)

    def test_export_refuses_insecure_sources_overwrite_and_symlinks(self) -> None:
        output = self.root / "insecure"
        status, _, stderr = self.invoke(
            "zed-to-nix",
            "--manifest",
            str(self.manifest),
            "--metadata",
            str(self.metadata),
            "--nixpkgs-lock",
            str(self.nixpkgs_lock),
            "--out-dir",
            str(output),
        )
        self.assertEqual(status, 2)
        self.assertIn("require --allow-local-source", stderr)
        self.assertEqual(self.export(output)[0], 0)
        status, _, stderr = self.export(output)
        self.assertEqual(status, 2)
        self.assertIn("--force", stderr)
        self.assertEqual(self.export(output, "--force")[0], 0)

        symlink_root = self.root / "symlink"
        symlink_root.mkdir()
        victim = self.root / "victim"
        victim.write_text("keep", encoding="utf-8")
        (symlink_root / "flake.nix").symlink_to(victim)
        status, _, stderr = self.export(symlink_root, "--force")
        self.assertEqual(status, 2)
        self.assertIn("symlink", stderr)
        self.assertEqual(victim.read_text(), "keep")

    def test_closure_free_nix_output_seals_deterministically_without_nix_runtime(self) -> None:
        first = self.root / "sealed-first"
        second = self.root / "sealed-second"
        status, stdout, stderr = self.seal(first)
        self.assertEqual(status, 0, stderr)
        self.assertEqual(json.loads(stdout)["direction"], "nix-to-zed")
        self.assertEqual(self.seal(second)[0], 0)
        artifact = first / "nix-portable-1.0.0.tar.gz"
        self.assertEqual(artifact.read_bytes(), (second / artifact.name).read_bytes())
        status, _, stderr = self.invoke("verify", "--directory", str(first))
        self.assertEqual(status, 0, stderr)
        with tarfile.open(artifact, "r:gz") as archive:
            names = set(archive.getnames())
            self.assertIn(".zpkg.toml", names)
            self.assertIn("zed-nix-adapter.json", names)
            self.assertIn("bin/portable", names)
            adapter = json.load(archive.extractfile("zed-nix-adapter.json"))
            self.assertFalse(adapter["policy"]["nix_required_at_zed_runtime"])
            manifest = archive.extractfile(".zpkg.toml").read().decode()
            self.assertIn('"portable" = "bin/portable"', manifest)

    def test_import_rejects_references_store_strings_and_unsafe_symlinks(self) -> None:
        self.write_path_info(references=["/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc"])
        status, _, stderr = self.seal(self.root / "references")
        self.assertEqual(status, 2)
        self.assertIn("portable import rejected Nix references", stderr)

        self.write_path_info()
        (self.output / "share/store.txt").write_text(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc\n", encoding="utf-8"
        )
        status, _, stderr = self.seal(self.root / "store-string")
        self.assertEqual(status, 2)
        self.assertIn("runtime store reference", stderr)
        (self.output / "share/store.txt").unlink()

        (self.output / "escape").symlink_to("../../outside")
        status, _, stderr = self.seal(self.root / "symlink")
        self.assertEqual(status, 2)
        self.assertIn("symlink escapes", stderr)

    def test_import_rejects_mutable_ref_missing_source_and_bad_bin(self) -> None:
        status, _, stderr = self.seal(
            self.root / "mutable", "--locked-ref", "github:acme/portable/main"
        )
        self.assertEqual(status, 2)
        self.assertIn("immutable", stderr)
        status, _, stderr = self.invoke(
            "nix-to-zed",
            "--store-path",
            str(self.output),
            "--path-info",
            str(self.path_info),
            "--derivation-json",
            str(self.derivation),
            "--flake-lock",
            str(self.import_lock),
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
            "missing=bin/missing",
            "--repository",
            "https://github.com/acme/portable",
            "--source-revision",
            REVISION,
            "--license",
            "MIT",
            "--description",
            "fixture",
            "--nix-version",
            "nix 2.31",
            "--out-dir",
            str(self.root / "missing-source"),
            "--allow-local-store",
        )
        self.assertEqual(status, 2)
        self.assertTrue("does not resolve inside" in stderr or "missing or not executable" in stderr or "requires --source-available" in stderr)

    def test_verify_rejects_artifact_and_sidecar_tamper(self) -> None:
        output = self.root / "tamper"
        self.assertEqual(self.seal(output)[0], 0)
        artifact = output / "nix-portable-1.0.0.tar.gz"
        artifact.write_bytes(artifact.read_bytes() + b"tamper")
        status, _, stderr = self.invoke("verify", "--directory", str(output))
        self.assertEqual(status, 2)
        self.assertIn("SHA-256 mismatch", stderr)

        output = self.root / "sidecar"
        self.assertEqual(self.seal(output)[0], 0)
        bridge_path = output / "bridge.json"
        value = json.loads(bridge_path.read_text())
        value["policy"]["nix_required_at_zed_runtime"] = True
        bridge_path.write_text(json.dumps(value), encoding="utf-8")
        status, _, stderr = self.invoke("verify", "--directory", str(output))
        self.assertEqual(status, 2)
        self.assertIn("embedded adapter/sidecar mismatch", stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
