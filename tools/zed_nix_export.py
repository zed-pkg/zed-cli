from __future__ import annotations

from zed_nix_common import *  # noqa: F403


def render_package_nix(
    *,
    org: str,
    name: str,
    version: str,
    description: str,
    license_name: str,
    artifact_url: str,
    artifact_sri: str,
    artifact_format: str,
    systems: Sequence[str],
    bins: Mapping[str, str],
    vcs_tag: str,
    vcs_commit: str,
) -> str:
    native_inputs = "[ unzip ]" if artifact_format == "zip" else "[ ]"
    unpack = (
        'unzip -q "$src" -d source'
        if artifact_format == "zip"
        else 'tar -xzf "$src" -C source'
    )
    bin_lines: list[str] = []
    if bins:
        bin_lines.append('mkdir -p "$out/bin"')
        for binary, relative in bins.items():
            source = f'$packageRoot/{relative}'
            bin_lines.extend(
                [
                    f'if [ ! -x "{source}" ]; then',
                    f'  echo "declared Zed binary is missing or not executable: {relative}" >&2',
                    "  exit 1",
                    "fi",
                    f'ln -s "{source}" "$out/bin/{binary}"',
                ]
            )
    else:
        bin_lines.append("# Data/library artifact: no executable entries declared.")
    bin_body = "\n    ".join(bin_lines)
    platform_values = " ".join(nix_string(system) for system in systems)
    free = "false" if license_name in {"UNLICENSED", "proprietary"} else "true"
    return f'''{{ stdenvNoCC, fetchurl, unzip }}:

stdenvNoCC.mkDerivation {{
  pname = {nix_string(name)};
  version = {nix_string(version)};

  src = fetchurl {{
    url = {nix_string(artifact_url)};
    hash = {nix_string(artifact_sri)};
  }};

  nativeBuildInputs = {native_inputs};
  dontConfigure = true;
  dontBuild = true;

  unpackPhase = ''
    runHook preUnpack
    mkdir source
    {unpack}
    cd source
    runHook postUnpack
  '';

  installPhase = ''
    runHook preInstall
    packageRoot="$out/share/zed-pkg/{org}/{name}"
    mkdir -p "$packageRoot"
    cp -a . "$packageRoot/"
    {bin_body}
    runHook postInstall
  '';

  passthru.zed = {{
    package = {nix_string(f"{org}/{name}")};
    version = {nix_string(version)};
    artifactHash = {nix_string(artifact_sri)};
    vcsTag = {nix_string(vcs_tag)};
    vcsCommit = {nix_string(vcs_commit)};
  }};

  meta = {{
    description = {nix_string(description)};
    license = {{
      fullName = {nix_string(license_name)};
      spdxId = {nix_string(license_name)};
      free = {free};
    }};
    platforms = [ {platform_values} ];
  }};
}}
'''


def render_flake(name: str, systems: Sequence[str], nixpkgs_url: str) -> str:
    system_lines = "\n        ".join(nix_string(system) for system in systems)
    return f'''{{
  description = {nix_string(f"Standalone Nix export for Zed package {name}")};

  inputs.nixpkgs.url = {nix_string(nixpkgs_url)};

  outputs =
    {{ self, nixpkgs }}:
    let
      systems = [
        {system_lines}
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {{
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {{ inherit system; }};
          package = pkgs.callPackage ./nix/package.nix {{ }};
        in
        {{
          default = package;
          {nix_string(name)} = package;
        }}
      );
      checks = forAllSystems (system: {{ default = self.packages.${{system}}.default; }});
    }};
}}
'''


def export_readme(org: str, name: str, version: str) -> str:
    return f"""# Standalone Nix export of `{org}/{name}@{version}`

This bundle was generated from one immutable Zed registry artifact. Zed was the
resolution authority; Nix receives only a fixed URL/hash and does not resolve
Zed semver requirements.

```sh
nix flake check --no-update-lock-file
nix build --no-update-lock-file
```

The exact artifact tree is installed at `$out/share/zed-pkg/{org}/{name}`.
`zed-nix-adapter.json` records the translation inputs and generated-file hashes.
"""


def command_zed_to_nix(args: argparse.Namespace) -> None:
    manifest = read_toml(args.manifest.resolve())
    metadata = read_json(args.metadata.resolve())
    org, name, version, repository = package_identity(manifest)
    ensure_dependency_free(manifest)
    bins = collect_bins(manifest)
    systems = validate_systems(args.system or (() if bins else DEFAULT_SYSTEMS))

    for key, expected in (("org", org), ("name", name), ("version", version)):
        if metadata.get(key) != expected:
            fail(f"version metadata {key} mismatch: expected {expected!r}, found {metadata.get(key)!r}")
    artifact_sha = validate_hex(metadata.get("sha256"), "artifact sha256")
    artifact_sri = hex_to_sri(artifact_sha)
    artifact_format = metadata.get("format")
    if artifact_format not in {"tar.gz", "zip"}:
        fail(f"unsupported Zed artifact format {artifact_format!r}")
    artifact_url = validate_artifact_url(metadata.get("download_url"), args.allow_local_source)
    vcs_tag = validate_version(metadata.get("vcs_tag"), "version metadata vcs_tag")
    vcs_commit = metadata.get("vcs_commit")
    if not isinstance(vcs_commit, str) or not REV_RE.fullmatch(vcs_commit):
        fail("version metadata vcs_commit must be an immutable 40-64 character revision")
    repository_url = repository.get("url")
    if not isinstance(repository_url, str) or not repository_url:
        fail("[package.repository].url must be non-empty")
    description = manifest["package"].get("description") or f"Zed package {org}/{name}"
    license_name = manifest["package"].get("license") or "UNLICENSED"
    lock_bytes, nixpkgs_revision, nixpkgs_nar_hash = validate_nixpkgs_lock(
        args.nixpkgs_lock.resolve(), args.nixpkgs_url
    )

    files: dict[str, tuple[bytes, int]] = {
        "README.md": (export_readme(org, name, version).encode(), 0o644),
        "flake.lock": (lock_bytes, 0o644),
        "flake.nix": (render_flake(name, systems, args.nixpkgs_url).encode(), 0o644),
        "nix/package.nix": (
            render_package_nix(
                org=org,
                name=name,
                version=version,
                description=str(description),
                license_name=str(license_name),
                artifact_url=artifact_url,
                artifact_sri=artifact_sri,
                artifact_format=str(artifact_format),
                systems=systems,
                bins=bins,
                vcs_tag=vcs_tag,
                vcs_commit=vcs_commit,
            ).encode(),
            0o644,
        ),
    }
    generated_hashes = {
        relative: sha256_bytes(content) for relative, (content, _) in files.items()
    }
    adapter = {
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "direction": "zed-to-nix",
        "package": {"org": org, "name": name, "version": version, "target": None},
        "zed": {
            "repository": repository_url,
            "vcs_tag": vcs_tag,
            "vcs_commit": vcs_commit,
            "artifact_url": artifact_url,
            "artifact_sha256": artifact_sha,
            "artifact_hash_sri": artifact_sri,
            "format": artifact_format,
        },
        "nix": {
            "nixpkgs_url": args.nixpkgs_url,
            "nixpkgs_revision": nixpkgs_revision,
            "nixpkgs_nar_hash": nixpkgs_nar_hash,
            "systems": list(systems),
            "attribute": f'packages.${{system}}."{name}"',
            "install_layout": f"share/zed-pkg/{org}/{name}",
        },
        "policy": {
            "profile": "strict-v1",
            "resolution_authority": "zed",
            "artifact_export": True,
            "dependency_graph": "empty",
            "arbitrary_build_command": False,
        },
        "generated_files": generated_hashes,
        "licenses": [str(license_name)],
    }
    files["zed-nix-adapter.json"] = (json_bytes(adapter), 0o644)
    write_files(args.out_dir.resolve(), files, args.force)
    print(json.dumps(adapter, indent=2, sort_keys=True))
