{ lib, stdenv, stdenvNoCC, cacert, coreutils, findutils, python3 }:

let
  proxyImpureEnvVars = [
    "http_proxy"
    "https_proxy"
    "ftp_proxy"
    "all_proxy"
    "no_proxy"
    "HTTP_PROXY"
    "HTTPS_PROXY"
    "FTP_PROXY"
    "ALL_PROXY"
    "NO_PROXY"
  ];
in
rec {
  # Resolve exactly the graph already pinned by .zpkg.lock and materialize its
  # copy-install result as a recursive fixed-output derivation. Nix verifies the
  # complete output NAR; zed-pkg remains the only dependency resolver.
  fetchZedDeps =
    { pname ? "zed-deps"
    , version ? "0"
    , src
    , zed
    , hash ? lib.fakeHash
    , registry ? null
    , registryPath ? null
    , manifestPath ? ".zpkg.toml"
    , lockfilePath ? ".zpkg.lock"
    , adapter ? "none"
    , target ? null
    }:
    let
      registryValue =
        if registryPath != null then "file://${toString registryPath}"
        else registry;
      registryArgs = lib.optionalString (registryValue != null)
        "--registry ${lib.escapeShellArg registryValue}";
      targetArgs = lib.optionalString (target != null)
        "--target ${lib.escapeShellArg target}";
      manifestArgs = lib.optionalString (manifestPath == null) "--skip-manifest";
      registryKind =
        if registryPath != null then "immutable-nix-store-input"
        else if registry != null then "explicit-url"
        else "default";
      bridgeMetadata = builtins.toJSON {
        schema = "zed.nix-fetch-bridge/v1";
        resolver_authority = "zed-pkg";
        install_mode = "copy";
        inherit adapter target;
        build_hooks = false;
        registry_kind = registryKind;
        registry_literal_retained = false;
        raw_lock_retained = false;
        raw_manifest_retained = false;
        lock_summary_schema = "zed.nix-lock-summary/v1";
        canonical_adapter_record = false;
      };
    in
    assert lib.assertMsg (registry == null || registryPath == null)
      "fetchZedDeps: set at most one of registry and registryPath";
    assert lib.assertMsg (!lib.hasInfix "/nix/store/" adapter)
      "fetchZedDeps: adapter metadata must not contain a Nix store path";
    assert lib.assertMsg (target == null || !lib.hasInfix "/nix/store/" target)
      "fetchZedDeps: target metadata must not contain a Nix store path";
    stdenvNoCC.mkDerivation {
      inherit pname version src;

      nativeBuildInputs = [ zed cacert coreutils findutils python3 ];
      phases = [ "installPhase" ];

      # An explicit path-valued attribute keeps immutable file registries in
      # the sandbox closure. Callers that already registered a path with Nix
      # should pass it through builtins.storePath so the URL frozen into the
      # lock remains the exact input identity. That identity is never copied
      # into the fixed output because FOD outputs must not retain store refs.
      ZED_PKG_NIX_REGISTRY_INPUT =
        if registryPath == null then "" else registryPath;

      # A recursive fixed-output derivation is the only network-enabled stage.
      # The regular package derivation below consumes this output offline.
      outputHashMode = "recursive";
      outputHashAlgo = "sha256";
      outputHash = hash;
      impureEnvVars = proxyImpureEnvVars;

      installPhase = ''
        runHook preInstall
        set -euo pipefail

        export HOME="$TMPDIR/home"
        export XDG_CACHE_HOME="$TMPDIR/xdg-cache"
        export XDG_CONFIG_HOME="$TMPDIR/xdg-config"
        export XDG_DATA_HOME="$TMPDIR/xdg-data"
        export ZED_PKG_HOME="$TMPDIR/zed-home"
        export ZED_PKG_INTERACTIVE=0
        export ZED_PKG_ALLOW_BUILD=0
        export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
        export NIX_SSL_CERT_FILE="$SSL_CERT_FILE"

        # Secrets are deliberately outside this first bridge. Private registry
        # support needs an explicit Nix credential/secret contract rather than
        # accidental inheritance into a reproducible derivation.
        unset ZED_PKG_TOKEN ZED_PKG_SUPABASE_KEY ZED_PKG_AUTH_PASSWORD || true

        mkdir -p \
          "$HOME" \
          "$XDG_CACHE_HOME" \
          "$XDG_CONFIG_HOME" \
          "$XDG_DATA_HOME" \
          "$ZED_PKG_HOME"

        ${lib.optionalString (registryPath != null) ''
          if [[ ! -d "$ZED_PKG_NIX_REGISTRY_INPUT" ]]; then
            echo "zed-pkg Nix bridge: immutable file registry input is missing: $ZED_PKG_NIX_REGISTRY_INPUT" >&2
            exit 1
          fi
        ''}

        work="$TMPDIR/zed-project"
        mkdir -p "$work"

        require_safe_relative() {
          local label="$1"
          local value="$2"
          case "$value" in
            ""|/*|.|..|../*|*/../*|*/..)
              echo "zed-pkg Nix bridge: $label must be a safe relative path, got: $value" >&2
              exit 1
              ;;
          esac
        }

        lockfile_path=${lib.escapeShellArg lockfilePath}
        require_safe_relative lockfilePath "$lockfile_path"
        if [[ ! -f "$src/$lockfile_path" ]]; then
          echo "zed-pkg Nix bridge: missing lockfile $lockfile_path in $src" >&2
          exit 1
        fi
        cp "$src/$lockfile_path" "$work/.zpkg.lock"
        input_lock_digest="$(sha256sum "$work/.zpkg.lock" | cut -d' ' -f1)"

        ${lib.optionalString (manifestPath != null) ''
          manifest_path=${lib.escapeShellArg manifestPath}
          require_safe_relative manifestPath "$manifest_path"
          if [[ ! -f "$src/$manifest_path" ]]; then
            echo "zed-pkg Nix bridge: missing manifest $manifest_path in $src" >&2
            exit 1
          fi
          cp "$src/$manifest_path" "$work/.zpkg.toml"
          input_manifest_digest="$(sha256sum "$work/.zpkg.toml" | cut -d' ' -f1)"
        ''}

        cd "$work"
        ${zed}/bin/zed ${registryArgs} install \
          --frozen \
          --install-mode copy \
          --adapter ${lib.escapeShellArg adapter} \
          ${targetArgs} \
          ${manifestArgs}

        installed_lock_digest="$(sha256sum "$work/.zpkg.lock" | cut -d' ' -f1)"
        if [[ "$installed_lock_digest" != "$input_lock_digest" ]]; then
          echo "zed-pkg Nix bridge: frozen install rewrote .zpkg.lock bytes; refusing non-frozen output" >&2
          exit 1
        fi

        ${lib.optionalString (manifestPath != null) ''
          installed_manifest_digest="$(sha256sum "$work/.zpkg.toml" | cut -d' ' -f1)"
          if [[ "$installed_manifest_digest" != "$input_manifest_digest" ]]; then
            echo "zed-pkg Nix bridge: frozen install rewrote .zpkg.toml bytes" >&2
            exit 1
          fi
        ''}

        if [[ -e "$work/.zpkg-staging" ]]; then
          echo "zed-pkg Nix bridge: successful install left transaction state behind" >&2
          exit 1
        fi

        # `.zed/operation.lock` is a durable local rendezvous point whose
        # diagnostic payload contains process/host/timing data. Descriptor
        # ownership—not those bytes—is authoritative. It must not enter a
        # recursive fixed output or identical frozen installs hash differently.
        operation_lock="$work/.zed/operation.lock"
        if [[ -L "$operation_lock" ]]; then
          echo "zed-pkg Nix bridge: operation lock must not be a symlink" >&2
          exit 1
        fi
        if [[ -e "$operation_lock" ]]; then
          if [[ ! -f "$operation_lock" ]]; then
            echo "zed-pkg Nix bridge: operation lock must be a regular file" >&2
            exit 1
          fi
          rm -f -- "$operation_lock"
          rmdir "$work/.zed" 2>/dev/null || true
        fi

        mkdir -p "$out/tree" "$out/metadata"
        shopt -s dotglob nullglob
        for entry in "$work"/*; do
          base="$(basename "$entry")"
          case "$base" in
            .zpkg.toml|.zpkg.lock|.zpkg-staging)
              continue
              ;;
          esac
          cp -a "$entry" "$out/tree/"
        done
        shopt -u dotglob nullglob

        # Raw manifests and locks are build inputs, not FOD outputs. A lock may
        # legitimately name an immutable file:///nix/store/... registry, while
        # Nix correctly rejects fixed outputs that retain references to another
        # store object. Preserve exact-byte evidence by digest and emit only a
        # deterministic, source-redacted package inventory.
        printf '%s\n' "$input_lock_digest" > "$out/metadata/lock.sha256"
        ${lib.optionalString (manifestPath != null) ''
          printf '%s\n' "$input_manifest_digest" > "$out/metadata/manifest.sha256"
        ''}
        printf '%s\n' ${lib.escapeShellArg bridgeMetadata} > "$out/metadata/bridge.json"

        python3 - "$work/.zpkg.lock" "$out/metadata/lock-summary.json" "$input_lock_digest" <<'PY'
        import hashlib
        import json
        import pathlib
        import re
        import sys
        import tomllib

        lock_path = pathlib.Path(sys.argv[1])
        output_path = pathlib.Path(sys.argv[2])
        expected_digest = sys.argv[3]
        raw = lock_path.read_bytes()
        actual_digest = hashlib.sha256(raw).hexdigest()
        if actual_digest != expected_digest:
            raise SystemExit("lock digest changed before summary generation")

        data = tomllib.loads(raw.decode("utf-8"))
        packages = data.get("package", [])
        if not isinstance(packages, list):
            raise SystemExit("lock package field must be an array")

        def safe_text(value, label):
            if not isinstance(value, str) or not value:
                raise SystemExit(f"{label} must be a non-empty string")
            if "/nix/store/" in value:
                raise SystemExit(f"{label} must not contain a Nix store path")
            return value

        def source_kind(value):
            if not isinstance(value, str):
                raise SystemExit("package source must be a string")
            if value.startswith("file:///nix/store/"):
                return "immutable-nix-store-input"
            if value.startswith("file://"):
                return "file"
            if value.startswith("https://"):
                return "https"
            if value.startswith("http://"):
                return "http"
            return "other"

        normalized = []
        for index, package in enumerate(packages):
            if not isinstance(package, dict):
                raise SystemExit(f"package[{index}] must be a table")
            digest = package.get("sha256")
            if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
                raise SystemExit(f"package[{index}].sha256 must be 64 lowercase hex characters")
            size = package.get("size")
            if not isinstance(size, int) or isinstance(size, bool) or size < 0:
                raise SystemExit(f"package[{index}].size must be a non-negative integer")
            normalized.append({
                "format": safe_text(package.get("format", "tar.gz"), f"package[{index}].format"),
                "name": safe_text(package.get("name"), f"package[{index}].name"),
                "org": safe_text(package.get("org"), f"package[{index}].org"),
                "sha256": digest,
                "size": size,
                "source_kind": source_kind(package.get("source")),
                "version": safe_text(package.get("version"), f"package[{index}].version"),
            })

        normalized.sort(key=lambda package: (
            package["org"],
            package["name"],
            package["version"],
            package["sha256"],
        ))
        adapters = data.get("nix-adapter", [])
        if not isinstance(adapters, list):
            raise SystemExit("lock nix-adapter field must be an array")

        summary = {
            "lockfile_version": data.get("version"),
            "nix_adapter_count": len(adapters),
            "package_count": len(normalized),
            "packages": normalized,
            "raw_lock_sha256": actual_digest,
            "schema": "zed.nix-lock-summary/v1",
            "source_literals_retained": False,
        }
        output_path.write_text(
            json.dumps(summary, sort_keys=True, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        PY

        ${zed}/bin/zed --version > "$out/metadata/zed-version.txt"

        # Copy mode may preserve package-owned relative links, but the fixed
        # output must never retain an absolute or escaping dependency on the
        # temporary builder or another undeclared store path.
        while IFS= read -r -d $'\0' link; do
          link_target="$(readlink "$link")"
          if [[ "$link_target" = /* ]]; then
            echo "zed-pkg Nix bridge: absolute symlink is not exportable: $link -> $link_target" >&2
            exit 1
          fi
          resolved="$(readlink -m "$(dirname "$link")/$link_target")"
          case "$resolved" in
            "$out/tree"|"$out/tree/"*) ;;
            *)
              echo "zed-pkg Nix bridge: symlink escapes materialized tree: $link -> $link_target" >&2
              exit 1
              ;;
          esac
          if [[ ! -e "$link" ]]; then
            echo "zed-pkg Nix bridge: broken symlink is not exportable: $link -> $link_target" >&2
            exit 1
          fi
        done < <(find "$out/tree" -type l -print0)

        runHook postInstall
      '';
    };

  # Build an ordinary, network-isolated Nix package from a source tree plus the
  # verified fixed output above. Callers retain the complete stdenv surface.
  mkZedPackage =
    args@{
      zedDeps,
      postUnpack ? "",
      postPatch ? "",
      passthru ? { },
      ...
    }:
    stdenv.mkDerivation (
      (builtins.removeAttrs args [
        "zedDeps"
        "postUnpack"
        "postPatch"
        "passthru"
      ])
      // {
        ZED_PKG_DEPS = zedDeps;
        postUnpack = ''
          # Directory sources copied from the Nix store may retain read-only
          # modes on Darwin. The verified dependency overlay and normal stdenv
          # phases require a writable private build-tree copy.
          if [[ -n "''${sourceRoot:-}" && -d "$sourceRoot" ]]; then
            chmod -R u+w "$sourceRoot"
          fi
          ${postUnpack}
        '';
        postPatch = ''
          if [[ ! -d ${zedDeps}/tree ]]; then
            echo "mkZedPackage: zedDeps does not contain the Nix bridge tree" >&2
            exit 1
          fi
          cp -a ${zedDeps}/tree/. .
          ${postPatch}
        '';
        passthru = passthru // { inherit zedDeps; };
      }
    );
}
