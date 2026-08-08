#!/usr/bin/env python3
"""Publish a checksum-verified release directory to private Cloudflare R2.

The implementation uses only Python's standard library and AWS Signature V4.
Credentials are read from the environment and are never written to reports.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import hmac
import json
import mimetypes
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable, Mapping, Protocol

EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
BUCKET_RE = re.compile(r"^[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]$")
ACCOUNT_RE = re.compile(r"^[0-9a-f]{32}$")


class PublicationError(RuntimeError):
    """A fail-closed publication error."""


def _hmac(key: bytes, value: str) -> bytes:
    return hmac.new(key, value.encode("utf-8"), hashlib.sha256).digest()


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _canonical_uri(path: str) -> str:
    if not path.startswith("/"):
        raise PublicationError("request path must be absolute")
    return "/".join(urllib.parse.quote(part, safe="-_.~") for part in path.split("/"))


def _normalize_header_value(value: str) -> str:
    return " ".join(value.strip().split())


def _validate_bucket(bucket: str) -> str:
    if not BUCKET_RE.fullmatch(bucket) or ".." in bucket or ".-" in bucket or "-." in bucket:
        raise PublicationError(f"invalid R2 bucket name: {bucket!r}")
    return bucket


def _validate_prefix(prefix: str) -> str:
    value = prefix.strip("/")
    if not value:
        raise PublicationError("object prefix must not be empty")
    parts = value.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        raise PublicationError(f"unsafe object prefix: {prefix!r}")
    return value


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def collect_files(directory: Path) -> list[Path]:
    root = directory.resolve(strict=True)
    if not root.is_dir():
        raise PublicationError(f"publication source is not a directory: {root}")
    files: list[Path] = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise PublicationError(f"publication source contains a symlink: {path}")
        if path.is_file():
            files.append(path)
        elif not path.is_dir():
            raise PublicationError(f"publication source contains a special file: {path}")
    if not files:
        raise PublicationError(f"publication source is empty: {root}")
    return files


@dataclass(frozen=True)
class ObjectMetadata:
    content_length: int
    sha256: str | None


class ObjectStore(Protocol):
    def bucket_exists(self, bucket: str) -> bool: ...

    def create_bucket(self, bucket: str) -> None: ...

    def head_object(self, bucket: str, key: str) -> ObjectMetadata | None: ...

    def put_object(
        self,
        bucket: str,
        key: str,
        body: bytes,
        *,
        content_type: str,
        cache_control: str,
        metadata: Mapping[str, str],
    ) -> None: ...


class R2Client:
    """Minimal path-style S3 client for Cloudflare R2."""

    def __init__(
        self,
        *,
        account_id: str,
        access_key_id: str,
        secret_access_key: str,
        now: Callable[[], dt.datetime] | None = None,
    ) -> None:
        if not ACCOUNT_RE.fullmatch(account_id):
            raise PublicationError("CLOUDFLARE_ACCOUNT_ID must be 32 lowercase hex characters")
        if not access_key_id or not secret_access_key:
            raise PublicationError("R2 access-key credentials must not be empty")
        self.account_id = account_id
        self.access_key_id = access_key_id
        self.secret_access_key = secret_access_key
        self.endpoint = f"https://{account_id}.r2.cloudflarestorage.com"
        self.host = f"{account_id}.r2.cloudflarestorage.com"
        self._now = now or (lambda: dt.datetime.now(dt.timezone.utc))

    def _authorization(
        self,
        *,
        method: str,
        canonical_uri: str,
        headers: Mapping[str, str],
        payload_hash: str,
        timestamp: dt.datetime,
    ) -> str:
        normalized = {name.lower(): _normalize_header_value(value) for name, value in headers.items()}
        canonical_headers = "".join(f"{name}:{normalized[name]}\n" for name in sorted(normalized))
        signed_headers = ";".join(sorted(normalized))
        canonical_request = "\n".join(
            [method, canonical_uri, "", canonical_headers, signed_headers, payload_hash]
        )
        date = timestamp.strftime("%Y%m%d")
        amz_date = timestamp.strftime("%Y%m%dT%H%M%SZ")
        scope = f"{date}/auto/s3/aws4_request"
        string_to_sign = "\n".join(
            ["AWS4-HMAC-SHA256", amz_date, scope, _sha256(canonical_request.encode("utf-8"))]
        )
        date_key = _hmac(("AWS4" + self.secret_access_key).encode("utf-8"), date)
        region_key = _hmac(date_key, "auto")
        service_key = _hmac(region_key, "s3")
        signing_key = _hmac(service_key, "aws4_request")
        signature = hmac.new(signing_key, string_to_sign.encode("utf-8"), hashlib.sha256).hexdigest()
        return (
            "AWS4-HMAC-SHA256 "
            f"Credential={self.access_key_id}/{scope},"
            f"SignedHeaders={signed_headers},Signature={signature}"
        )

    def _request(
        self,
        method: str,
        path: str,
        *,
        body: bytes = b"",
        headers: Mapping[str, str] | None = None,
    ) -> tuple[int, Mapping[str, str], bytes]:
        canonical_uri = _canonical_uri(path)
        timestamp = self._now().astimezone(dt.timezone.utc)
        payload_hash = _sha256(body)
        signed = {
            "host": self.host,
            "x-amz-content-sha256": payload_hash,
            "x-amz-date": timestamp.strftime("%Y%m%dT%H%M%SZ"),
        }
        for name, value in (headers or {}).items():
            signed[name.lower()] = value
        authorization = self._authorization(
            method=method,
            canonical_uri=canonical_uri,
            headers=signed,
            payload_hash=payload_hash,
            timestamp=timestamp,
        )
        request_headers = {**signed, "authorization": authorization}
        request = urllib.request.Request(
            self.endpoint + canonical_uri,
            data=body if method in {"PUT", "POST"} else None,
            headers=request_headers,
            method=method,
        )
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                return response.status, dict(response.headers.items()), response.read()
        except urllib.error.HTTPError as error:
            response_body = error.read()
            if error.code == 404:
                return error.code, dict(error.headers.items()), response_body
            detail = response_body.decode("utf-8", errors="replace")[:500]
            raise PublicationError(f"R2 {method} {path} failed with HTTP {error.code}: {detail}") from error
        except urllib.error.URLError as error:
            raise PublicationError(f"R2 {method} {path} failed: {error.reason}") from error

    def bucket_exists(self, bucket: str) -> bool:
        status, _, _ = self._request("HEAD", f"/{_validate_bucket(bucket)}")
        return status != 404

    def create_bucket(self, bucket: str) -> None:
        status, _, _ = self._request("PUT", f"/{_validate_bucket(bucket)}")
        if status not in {200, 201, 204}:
            raise PublicationError(f"unexpected CreateBucket status: {status}")

    def head_object(self, bucket: str, key: str) -> ObjectMetadata | None:
        status, headers, _ = self._request("HEAD", f"/{_validate_bucket(bucket)}/{key}")
        if status == 404:
            return None
        lowered = {name.lower(): value for name, value in headers.items()}
        return ObjectMetadata(
            content_length=int(lowered.get("content-length", "0")),
            sha256=lowered.get("x-amz-meta-sha256"),
        )

    def put_object(
        self,
        bucket: str,
        key: str,
        body: bytes,
        *,
        content_type: str,
        cache_control: str,
        metadata: Mapping[str, str],
    ) -> None:
        headers = {
            "content-type": content_type,
            "cache-control": cache_control,
            **{f"x-amz-meta-{name}": value for name, value in metadata.items()},
        }
        status, _, _ = self._request(
            "PUT", f"/{_validate_bucket(bucket)}/{key}", body=body, headers=headers
        )
        if status not in {200, 201, 204}:
            raise PublicationError(f"unexpected PutObject status for {key}: {status}")


def publish_directory(
    *,
    store: ObjectStore,
    directory: Path,
    bucket: str,
    prefix: str,
    release: str,
    source_repository: str,
    source_run_id: str,
    create_bucket: bool,
    overwrite: bool,
) -> dict[str, object]:
    bucket = _validate_bucket(bucket)
    prefix = _validate_prefix(prefix)
    root = directory.resolve(strict=True)
    files = collect_files(root)

    if not store.bucket_exists(bucket):
        if not create_bucket:
            raise PublicationError(
                f"R2 bucket {bucket!r} does not exist; create it explicitly or pass --create-bucket"
            )
        store.create_bucket(bucket)
        if not store.bucket_exists(bucket):
            raise PublicationError(f"R2 bucket creation did not become visible: {bucket!r}")

    results: list[dict[str, object]] = []
    for path in files:
        relative = path.relative_to(root).as_posix()
        key = f"{prefix}/{relative}"
        digest = _file_sha256(path)
        size = path.stat().st_size
        existing = store.head_object(bucket, key)
        if existing is not None:
            if existing.sha256 == digest and existing.content_length == size:
                results.append(
                    {"key": key, "sha256": digest, "size": size, "status": "unchanged"}
                )
                continue
            if not overwrite:
                raise PublicationError(
                    f"refusing to replace non-identical R2 object s3://{bucket}/{key}"
                )

        content_type = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
        body = path.read_bytes()
        store.put_object(
            bucket,
            key,
            body,
            content_type=content_type,
            cache_control="private, max-age=31536000, immutable",
            metadata={
                "sha256": digest,
                "release": release,
                "source-repository": source_repository,
                "source-run-id": source_run_id,
            },
        )
        verified = store.head_object(bucket, key)
        if verified is None or verified.sha256 != digest or verified.content_length != size:
            raise PublicationError(f"post-upload verification failed for s3://{bucket}/{key}")
        results.append({"key": key, "sha256": digest, "size": size, "status": "uploaded"})

    return {
        "schema": "https://zpkg.net/schemas/r2-publication-report/v1",
        "bucket": bucket,
        "prefix": prefix,
        "release": release,
        "source_repository": source_repository,
        "source_run_id": source_run_id,
        "objects": results,
    }


def dry_run_report(
    *,
    directory: Path,
    bucket: str,
    prefix: str,
    release: str,
    source_repository: str,
    source_run_id: str,
) -> dict[str, object]:
    root = directory.resolve(strict=True)
    objects = []
    for path in collect_files(root):
        relative = path.relative_to(root).as_posix()
        objects.append(
            {
                "key": f"{_validate_prefix(prefix)}/{relative}",
                "sha256": _file_sha256(path),
                "size": path.stat().st_size,
                "status": "planned",
            }
        )
    return {
        "schema": "https://zpkg.net/schemas/r2-publication-report/v1",
        "bucket": _validate_bucket(bucket),
        "prefix": _validate_prefix(prefix),
        "release": release,
        "source_repository": source_repository,
        "source_run_id": source_run_id,
        "objects": objects,
    }


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--directory", type=Path, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--prefix", required=True)
    parser.add_argument("--release", required=True)
    parser.add_argument("--source-repository", required=True)
    parser.add_argument("--source-run-id", required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--create-bucket", action="store_true")
    parser.add_argument("--overwrite", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    args = _parser().parse_args(argv)
    try:
        if args.dry_run:
            report = dry_run_report(
                directory=args.directory,
                bucket=args.bucket,
                prefix=args.prefix,
                release=args.release,
                source_repository=args.source_repository,
                source_run_id=args.source_run_id,
            )
        else:
            client = R2Client(
                account_id=os.environ.get("CLOUDFLARE_ACCOUNT_ID", ""),
                access_key_id=os.environ.get("R2_ACCESS_KEY_ID", ""),
                secret_access_key=os.environ.get("R2_SECRET_ACCESS_KEY", ""),
            )
            report = publish_directory(
                store=client,
                directory=args.directory,
                bucket=args.bucket,
                prefix=args.prefix,
                release=args.release,
                source_repository=args.source_repository,
                source_run_id=args.source_run_id,
                create_bucket=args.create_bucket,
                overwrite=args.overwrite,
            )
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(json.dumps(report, indent=2, sort_keys=True))
        return 0
    except (OSError, PublicationError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
