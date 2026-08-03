from __future__ import annotations

from zed_nix_common import *  # noqa: F403


def verify_hashes(root: Path, generated: object) -> None:
    if not isinstance(generated, Mapping) or not generated:
        fail("generated_files evidence must be non-empty")
    for relative, expected in generated.items():
        relative = safe_relative(relative, "generated file")
        expected = validate_hex(expected, f"generated hash for {relative}")
        path = root / relative
        if path.is_symlink() or not path.is_file():
            fail(f"generated file is missing or a symlink: {relative}")
        actual = sha256_file(path)
        if actual != expected:
            fail(f"generated file hash mismatch for {relative}: expected {expected}, found {actual}")


def safe_tar_members(archive: tarfile.TarFile) -> list[tarfile.TarInfo]:
    members = archive.getmembers()
    names: set[str] = set()
    for member in members:
        name = member.name.rstrip("/")
        safe_relative(name, "archive path")
        if name in names:
            fail(f"duplicate path in sealed artifact: {name}")
        names.add(name)
        if not (member.isfile() or member.isdir() or member.issym()):
            fail(f"unsupported member type in sealed artifact: {name}")
        if member.issym():
            target = member.linkname
            if os.path.isabs(target) or "/nix/store/" in target:
                fail(f"unsafe symlink in sealed artifact: {name} -> {target}")
            resolved = PurePosixPath(name).parent.joinpath(target)
            if any(part == ".." for part in resolved.parts):
                fail(f"escaping symlink in sealed artifact: {name} -> {target}")
    return members


def verify_sealed(root: Path, bridge: Mapping[str, Any]) -> None:
    artifact_info = bridge.get("artifact")
    if not isinstance(artifact_info, Mapping):
        fail("sealed bridge has no artifact evidence")
    artifact_name = safe_relative(artifact_info.get("file"), "artifact file")
    artifact = root / artifact_name
    expected_hash = validate_hex(artifact_info.get("sha256"), "sealed artifact hash")
    if artifact.is_symlink() or not artifact.is_file():
        fail("sealed artifact is missing or a symlink")
    if sha256_file(artifact) != expected_hash:
        fail("sealed artifact SHA-256 mismatch")
    if artifact.stat().st_size != artifact_info.get("size"):
        fail("sealed artifact size mismatch")

    embedded_adapter: bytes | None = None
    manifest_found = False
    with tarfile.open(artifact, mode="r:gz") as archive:
        for member in safe_tar_members(archive):
            if member.isfile():
                stream = archive.extractfile(member)
                if stream is None:
                    fail(f"could not read sealed member {member.name}")
                content = stream.read()
                match = STORE_REF_RE.search(content)
                if match:
                    fail(
                        f"sealed artifact embeds runtime store reference in {member.name}: "
                        f"{match.group(0).decode('ascii', errors='replace')}"
                    )
                if member.name == ".zpkg.toml":
                    manifest_found = True
                    tomllib.loads(content.decode("utf-8"))
                elif member.name == "zed-nix-adapter.json":
                    embedded_adapter = content
    if not manifest_found or embedded_adapter is None:
        fail("sealed artifact lacks .zpkg.toml or zed-nix-adapter.json")
    if sha256_bytes(embedded_adapter) != artifact_info.get("embedded_adapter_sha256"):
        fail("embedded adapter digest mismatch")
    embedded = json.loads(embedded_adapter)
    for key in ("schema", "schema_version", "direction", "package", "nix", "source", "policy"):
        if embedded.get(key) != bridge.get(key):
            fail(f"embedded adapter/sidecar mismatch for {key}")
    if bridge.get("policy", {}).get("nix_required_at_zed_runtime") is not False:
        fail("sealed package must not require Nix at Zed runtime")
    references = bridge.get("nix", {}).get("references")
    store_path = bridge.get("nix", {}).get("store_path")
    if not isinstance(references, list) or [item for item in references if item != store_path]:
        fail("sealed package provenance contains unresolved Nix references")


def command_verify(args: argparse.Namespace) -> None:
    root = args.directory.resolve()
    if (root / "zed-nix-adapter.json").is_file():
        adapter = read_json(root / "zed-nix-adapter.json")
        if adapter.get("schema") != SCHEMA or adapter.get("schema_version") != SCHEMA_VERSION:
            fail("unknown Zed/Nix adapter schema")
        if adapter.get("direction") != "zed-to-nix":
            fail("directory adapter direction is not zed-to-nix")
        verify_hashes(root, adapter.get("generated_files"))
        validate_sri(adapter.get("zed", {}).get("artifact_hash_sri"), "artifact SRI")
        validate_systems(adapter.get("nix", {}).get("systems") or [])
        result = adapter
    elif (root / "bridge.json").is_file():
        bridge = read_json(root / "bridge.json")
        if bridge.get("schema") != SCHEMA or bridge.get("schema_version") != SCHEMA_VERSION:
            fail("unknown Zed/Nix adapter schema")
        if bridge.get("direction") != "nix-to-zed":
            fail("sidecar bridge direction is not nix-to-zed")
        verify_sealed(root, bridge)
        result = bridge
    else:
        fail("directory has neither zed-nix-adapter.json nor bridge.json")
    print(json.dumps(result, indent=2, sort_keys=True))
