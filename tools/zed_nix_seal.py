from __future__ import annotations

from zed_nix_common import *  # noqa: F403


def parse_package_spec(value: str) -> tuple[str, str, str]:
    if "@" not in value or "/" not in value:
        fail("--as-package must be org/name@version")
    package, version = value.rsplit("@", 1)
    org, name = package.split("/", 1)
    return (
        validate_slug(org, "adapter org"),
        validate_slug(name, "adapter name"),
        validate_version(version, "adapter version"),
    )


def parse_bins(values: Sequence[str]) -> dict[str, str]:
    result: dict[str, str] = {}
    for value in values:
        if "=" not in value:
            fail(f"invalid --bin {value!r}; expected NAME=relative/path")
        name, path = value.split("=", 1)
        if not PROGRAM_RE.fullmatch(name):
            fail(f"invalid --bin name {name!r}")
        path = safe_relative(path, f"--bin {name}")
        if name in result:
            fail(f"duplicate --bin name {name}")
        result[name] = path
    return dict(sorted(result.items()))


def immutable_locked_ref(value: str) -> str:
    if value.startswith("github:"):
        body = value.removeprefix("github:").split("?", 1)[0]
        parts = body.split("/")
        if len(parts) == 3 and REV_RE.fullmatch(parts[2]):
            return value
    if value.startswith("git+https://") and "rev=" in value:
        revision = value.split("rev=", 1)[1].split("&", 1)[0]
        if REV_RE.fullmatch(revision):
            return value
    fail("--locked-ref must contain an immutable Git revision, not a branch/channel/path")


def path_info_record(value: object, store_path: str) -> Mapping[str, Any]:
    if isinstance(value, list):
        candidates = [item for item in value if isinstance(item, Mapping)]
    elif isinstance(value, Mapping):
        if store_path in value and isinstance(value[store_path], Mapping):
            record = dict(value[store_path])
            record.setdefault("path", store_path)
            return record
        candidates = [value]
    else:
        candidates = []
    for candidate in candidates:
        if candidate.get("path") == store_path:
            return candidate
    fail("path-info JSON has no record for the selected output")


def validate_store_tree(root: Path, bins: Mapping[str, str], allow_local: bool) -> list[str]:
    raw_root = str(root)
    if not allow_local and not raw_root.startswith("/nix/store/"):
        fail("strict Nix import requires a /nix/store output; fixtures need --allow-local-store")
    if root.is_symlink() or not root.is_dir():
        fail("selected Nix output must be a real directory, not a symlink")
    for reserved in RESERVED_SEALED_PATHS:
        if (root / reserved).exists() or (root / reserved).is_symlink():
            fail(f"Nix output collides with reserved sealed-package path {reserved}")

    discovered: list[str] = []
    for current, directories, files in os.walk(root, topdown=True, followlinks=False):
        current_path = Path(current)
        directories.sort()
        files.sort()
        names = list(directories) + list(files)
        for name in names:
            path = current_path / name
            relative = path.relative_to(root).as_posix()
            discovered.append(relative)
            info = os.lstat(path)
            mode = info.st_mode
            if stat.S_ISLNK(mode):
                target = os.readlink(path)
                if os.path.isabs(target) or "/nix/store/" in target:
                    fail(f"unsafe symlink in Nix output: {relative} -> {target}")
                resolved = (path.parent / target).resolve()
                try:
                    resolved.relative_to(root.resolve())
                except ValueError:
                    fail(f"symlink escapes Nix output: {relative} -> {target}")
                if path.name in directories:
                    directories.remove(path.name)
            elif stat.S_ISREG(mode):
                with path.open("rb") as stream:
                    tail = b""
                    for block in iter(lambda: stream.read(1024 * 1024), b""):
                        sample = tail + block
                        match = STORE_REF_RE.search(sample)
                        if match:
                            fail(
                                f"portable import rejected runtime store reference in {relative}: "
                                f"{match.group(0).decode('ascii', errors='replace')}"
                            )
                        tail = sample[-256:]
            elif stat.S_ISDIR(mode):
                continue
            else:
                fail(f"unsupported file type in Nix output: {relative}")

    for name, relative in bins.items():
        path = root / relative
        try:
            resolved = path.resolve(strict=True)
            resolved.relative_to(root.resolve())
        except (FileNotFoundError, ValueError):
            fail(f"declared --bin {name} does not resolve inside the selected output")
        if not resolved.is_file() or stat.S_IMODE(resolved.stat().st_mode) & 0o111 == 0:
            fail(f"declared --bin {name} is missing or not executable: {relative}")
    return discovered


def normalized_mode(mode: int, is_directory: bool = False) -> int:
    if is_directory:
        return 0o755
    return 0o755 if mode & 0o111 else 0o644


def tar_info(name: str, *, mode: int, size: int = 0) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name=name)
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = 0
    info.mode = mode
    info.size = size
    return info


def add_tree_to_tar(archive: tarfile.TarFile, root: Path) -> None:
    paths: list[Path] = []
    for current, directories, files in os.walk(root, topdown=True, followlinks=False):
        current_path = Path(current)
        directories.sort()
        files.sort()
        for name in directories + files:
            path = current_path / name
            paths.append(path)
            if path.is_symlink() and name in directories:
                directories.remove(name)
    for path in sorted(paths, key=lambda item: item.relative_to(root).as_posix()):
        relative = path.relative_to(root).as_posix()
        metadata = os.lstat(path)
        if stat.S_ISDIR(metadata.st_mode):
            info = tar_info(relative + "/", mode=normalized_mode(metadata.st_mode, True))
            info.type = tarfile.DIRTYPE
            archive.addfile(info)
        elif stat.S_ISLNK(metadata.st_mode):
            info = tar_info(relative, mode=0o777)
            info.type = tarfile.SYMTYPE
            info.linkname = os.readlink(path)
            archive.addfile(info)
        elif stat.S_ISREG(metadata.st_mode):
            info = tar_info(relative, mode=normalized_mode(metadata.st_mode), size=metadata.st_size)
            with path.open("rb") as stream:
                archive.addfile(info, stream)
        else:
            fail(f"unsupported file type while sealing {relative}")


def build_deterministic_tar(
    root: Path, extras: Mapping[str, tuple[bytes, int]], output: Path
) -> None:
    with output.open("wb") as raw:
        with gzip.GzipFile(
            filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=0
        ) as compressed:
            with tarfile.open(
                fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT
            ) as archive:
                add_tree_to_tar(archive, root)
                for relative, (content, mode) in sorted(extras.items()):
                    info = tar_info(relative, mode=mode, size=len(content))
                    archive.addfile(info, io.BytesIO(content))


def sealed_manifest(
    *,
    org: str,
    name: str,
    version: str,
    description: str,
    license_name: str,
    repository: str,
    bins: Mapping[str, str],
) -> str:
    bin_section = ""
    if bins:
        lines = ["\n[bin]"]
        for binary, path in bins.items():
            lines.append(f"{toml_string(binary)} = {toml_string(path)}")
        bin_section = "\n".join(lines) + "\n"
    return f'''[package]
org = {toml_string(org)}
name = {toml_string(name)}
version = {toml_string(version)}
description = {toml_string(description)}
license = {toml_string(license_name)}
keywords = ["nix", "sealed", "immutable"]

[package.repository]
vcs = "git"
url = {toml_string(repository)}
{bin_section}
[publish]
include_readme = true
tag_format = "v{{version}}"
'''


def command_nix_to_zed(args: argparse.Namespace) -> None:
    org, name, version = parse_package_spec(args.as_package)
    system = validate_system(args.system)
    output_name = validate_version(args.output, "--output")
    locked_ref = immutable_locked_ref(args.locked_ref)
    bins = parse_bins(args.bin)
    store_path = args.store_path.resolve()
    discovered = validate_store_tree(store_path, bins, args.allow_local_store)

    path_info_value = json.loads(args.path_info.read_text(encoding="utf-8"))
    record = path_info_record(path_info_value, str(store_path))
    nar_hash = validate_sri(record.get("narHash"), "path-info narHash")
    nar_size = record.get("narSize")
    if not isinstance(nar_size, int) or nar_size < 0:
        fail("path-info narSize must be a non-negative integer")
    references = record.get("references", [])
    if not isinstance(references, list) or not all(
        isinstance(item, str) for item in references
    ):
        fail("path-info references must be a string array")
    store_path_value = str(store_path)
    external_references = sorted(
        item for item in references if item != store_path_value
    )
    if external_references:
        fail("portable import rejected Nix references: " + ", ".join(external_references))
    signatures = record.get("signatures", record.get("sigs", []))
    if not isinstance(signatures, list) or not all(
        isinstance(item, str) for item in signatures
    ):
        fail("path-info signatures must be a string array")

    flake_lock_bytes = args.flake_lock.read_bytes()
    read_json(args.flake_lock)
    derivation_bytes = args.derivation_json.read_bytes()
    read_json(args.derivation_json)
    flake_lock_hash = sha256_bytes(flake_lock_bytes)
    derivation_hash = sha256_bytes(derivation_bytes)
    if not args.source_available:
        fail("publishable strict import requires --source-available")
    source_revision = args.source_revision.lower()
    if not REV_RE.fullmatch(source_revision):
        fail("--source-revision must be an immutable 40-64 character revision")
    repository = urlparse(args.repository)
    if repository.scheme != "https" or not repository.netloc:
        fail("--repository must be an HTTPS source URL")

    package = {"org": org, "name": name, "version": version, "target": None}
    source = {
        "repository": args.repository,
        "revision": source_revision,
        "available": True,
    }
    policy = {
        "profile": "strict-v1",
        "resolution_authority": "nix",
        "pure_evaluation": True,
        "import_from_derivation": False,
        "sandbox_required": True,
        "builder_network": "disabled",
        "dirty_source": False,
        "portable_reference_count": len(external_references),
        "nix_required_at_zed_runtime": False,
    }
    nix_evidence = {
        "locked_ref": locked_ref,
        "flake_lock_sha256": flake_lock_hash,
        "attribute": args.attribute,
        "system": system,
        "output": output_name,
        "derivation_json_sha256": derivation_hash,
        "store_path": store_path_value,
        "store_path_sha256": sha256_bytes(store_path_value.encode("utf-8")),
        "nar_hash": nar_hash,
        "nar_size": nar_size,
        "references": external_references,
        "signatures": sorted(signatures),
        "nix_version": args.nix_version,
        "store_info_json_version": 1,
    }
    embedded_nix_evidence = {
        key: value for key, value in nix_evidence.items() if key != "store_path"
    }
    embedded_adapter = {
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "direction": "nix-to-zed",
        "package": package,
        "nix": embedded_nix_evidence,
        "source": source,
        "policy": policy,
        "sealed_paths": discovered,
        "licenses": [args.license],
    }
    sidecar_core = {
        **embedded_adapter,
        "nix": nix_evidence,
    }
    adapter_bytes = json_bytes(embedded_adapter)
    manifest_bytes = sealed_manifest(
        org=org,
        name=name,
        version=version,
        description=args.description,
        license_name=args.license,
        repository=args.repository,
        bins=bins,
    ).encode()
    readme = (
        f"# `{org}/{name}@{version}`\n\n"
        "This artifact was deterministically sealed from one explicitly selected, "
        "closure-free Nix output. Ordinary Zed installation and execution do not "
        "require Nix. Origin and portability evidence is in "
        "`zed-nix-adapter.json`; the exact ephemeral store path remains only in "
        "the external `bridge.json` sidecar.\n"
    ).encode()
    extras = {
        ".zpkg.toml": (manifest_bytes, 0o644),
        "README.zed-nix.md": (readme, 0o644),
        "zed-nix-adapter.json": (adapter_bytes, 0o644),
    }

    out_dir = args.out_dir.resolve()
    artifact_name = f"{name}-{version}.tar.gz"
    artifact = out_dir / artifact_name
    bridge_path = out_dir / "bridge.json"
    preflight_outputs([artifact, bridge_path], args.force)
    out_dir.mkdir(parents=True, exist_ok=True)
    descriptor, temp_name = tempfile.mkstemp(prefix=f".{artifact_name}.", dir=out_dir)
    os.close(descriptor)
    temporary = Path(temp_name)
    try:
        build_deterministic_tar(store_path, extras, temporary)
        artifact_hash = sha256_file(temporary)
        artifact_size = temporary.stat().st_size
        bridge = {
            **sidecar_core,
            "artifact": {
                "format": "tar.gz",
                "file": artifact_name,
                "sha256": artifact_hash,
                "size": artifact_size,
                "embedded_adapter_sha256": sha256_bytes(adapter_bytes),
                "manifest_sha256": sha256_bytes(manifest_bytes),
            },
        }
        bridge_bytes = json_bytes(bridge)
        bridge_descriptor, bridge_temp_name = tempfile.mkstemp(
            prefix=".bridge.json.", dir=out_dir
        )
        bridge_temp = Path(bridge_temp_name)
        try:
            with os.fdopen(bridge_descriptor, "wb") as stream:
                stream.write(bridge_bytes)
                stream.flush()
                os.fsync(stream.fileno())
            os.chmod(temporary, 0o644)
            os.chmod(bridge_temp, 0o644)
            os.replace(temporary, artifact)
            os.replace(bridge_temp, bridge_path)
        finally:
            bridge_temp.unlink(missing_ok=True)
    finally:
        temporary.unlink(missing_ok=True)
    print(json.dumps(bridge, indent=2, sort_keys=True))
