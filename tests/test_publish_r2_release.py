from __future__ import annotations

import datetime as dt
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).parents[1] / "scripts" / "publish_r2_release.py"
SPEC = importlib.util.spec_from_file_location("publish_r2_release", MODULE_PATH)
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = module
SPEC.loader.exec_module(module)


class FakeStore:
    def __init__(self, *, bucket_exists: bool = True) -> None:
        self.exists = bucket_exists
        self.objects: dict[tuple[str, str], tuple[bytes, str]] = {}
        self.created: list[str] = []

    def bucket_exists(self, bucket: str) -> bool:
        return self.exists

    def create_bucket(self, bucket: str) -> None:
        self.created.append(bucket)
        self.exists = True

    def head_object(self, bucket: str, key: str):
        value = self.objects.get((bucket, key))
        if value is None:
            return None
        body, digest = value
        return module.ObjectMetadata(content_length=len(body), sha256=digest)

    def put_object(self, bucket: str, key: str, body: bytes, **_kwargs) -> None:
        self.objects[(bucket, key)] = (body, module._sha256(body))


class PublicationTests(unittest.TestCase):
    def fixture(self) -> tuple[tempfile.TemporaryDirectory[str], Path]:
        temporary = tempfile.TemporaryDirectory()
        root = Path(temporary.name)
        (root / "zed-linux.tar.gz").write_bytes(b"linux")
        (root / "SHA256SUMS").write_text("manifest\n")
        return temporary, root

    def test_signature_is_deterministic_and_scoped_to_r2_auto_region(self) -> None:
        timestamp = dt.datetime(2026, 8, 8, 12, 34, 56, tzinfo=dt.timezone.utc)
        client = module.R2Client(
            account_id="0" * 32,
            access_key_id="ACCESS",
            secret_access_key="SECRET",
            now=lambda: timestamp,
        )
        payload_hash = module._sha256(b"payload")
        headers = {
            "host": client.host,
            "x-amz-content-sha256": payload_hash,
            "x-amz-date": "20260808T123456Z",
            "content-type": "application/octet-stream",
            "x-amz-meta-sha256": payload_hash,
        }
        authorization = client._authorization(
            method="PUT",
            canonical_uri="/bucket/a%20b.tar.gz",
            headers=headers,
            payload_hash=payload_hash,
            timestamp=timestamp,
        )
        self.assertEqual(
            authorization,
            "AWS4-HMAC-SHA256 "
            "Credential=ACCESS/20260808/auto/s3/aws4_request,"
            "SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date;"
            "x-amz-meta-sha256,"
            "Signature=b67d84d9009a3c795169e093c59516349e86f7b9c954a2d96ba62ba6e675d16a",
        )

    def test_dry_run_contains_no_credentials(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        report = module.dry_run_report(
            directory=root,
            bucket="zed-pkg-releases",
            prefix="zed-cli/v0.1.0-rc.2",
            release="v0.1.0-rc.2",
            source_repository="zed-pkg/zed-cli",
            source_run_id="123",
        )
        serialized = json.dumps(report)
        self.assertEqual(len(report["objects"]), 2)
        self.assertNotIn("access", serialized.lower())
        self.assertNotIn("secret", serialized.lower())

    def test_publish_is_idempotent_for_identical_objects(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        store = FakeStore()
        first = module.publish_directory(
            store=store,
            directory=root,
            bucket="zed-pkg-releases",
            prefix="zed-cli/v0.1.0-rc.2",
            release="v0.1.0-rc.2",
            source_repository="zed-pkg/zed-cli",
            source_run_id="123",
            create_bucket=False,
            overwrite=False,
        )
        second = module.publish_directory(
            store=store,
            directory=root,
            bucket="zed-pkg-releases",
            prefix="zed-cli/v0.1.0-rc.2",
            release="v0.1.0-rc.2",
            source_repository="zed-pkg/zed-cli",
            source_run_id="123",
            create_bucket=False,
            overwrite=False,
        )
        self.assertTrue(all(item["status"] == "uploaded" for item in first["objects"]))
        self.assertTrue(all(item["status"] == "unchanged" for item in second["objects"]))

    def test_non_identical_object_fails_closed(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        store = FakeStore()
        key = "zed-cli/v0.1.0-rc.2/zed-linux.tar.gz"
        store.objects[("zed-pkg-releases", key)] = (b"different", module._sha256(b"different"))
        with self.assertRaises(module.PublicationError):
            module.publish_directory(
                store=store,
                directory=root,
                bucket="zed-pkg-releases",
                prefix="zed-cli/v0.1.0-rc.2",
                release="v0.1.0-rc.2",
                source_repository="zed-pkg/zed-cli",
                source_run_id="123",
                create_bucket=False,
                overwrite=False,
            )

    def test_missing_bucket_requires_explicit_creation(self) -> None:
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        store = FakeStore(bucket_exists=False)
        with self.assertRaises(module.PublicationError):
            module.publish_directory(
                store=store,
                directory=root,
                bucket="zed-pkg-releases",
                prefix="zed-cli/v0.1.0-rc.2",
                release="v0.1.0-rc.2",
                source_repository="zed-pkg/zed-cli",
                source_run_id="123",
                create_bucket=False,
                overwrite=False,
            )
        module.publish_directory(
            store=store,
            directory=root,
            bucket="zed-pkg-releases",
            prefix="zed-cli/v0.1.0-rc.2",
            release="v0.1.0-rc.2",
            source_repository="zed-pkg/zed-cli",
            source_run_id="123",
            create_bucket=True,
            overwrite=False,
        )
        self.assertEqual(store.created, ["zed-pkg-releases"])

    def test_symlink_is_rejected(self) -> None:
        if not hasattr(Path, "symlink_to"):
            self.skipTest("symlinks unsupported")
        temporary, root = self.fixture()
        self.addCleanup(temporary.cleanup)
        target = root / "zed-linux.tar.gz"
        link = root / "alias.tar.gz"
        try:
            link.symlink_to(target.name)
        except OSError:
            self.skipTest("symlinks unavailable")
        with self.assertRaises(module.PublicationError):
            module.collect_files(root)


if __name__ == "__main__":
    unittest.main()
