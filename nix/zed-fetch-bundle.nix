{ lib, stdenvNoCC, cacert, coreutils, findutils, python3 }:

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
{
  # Fetch and verify the exact artifact graph already pinned by .zpkg.lock.
  # Unlike fetchZedDeps, this output is not installation-shaped: it is the
  # source-redacted zed.fetch/v1 bundle emitted by `zed fetch --frozen`.
  fetchZedArtifacts =
    { pname ? "zed-artifacts"
    , version ? "0"
    , src
    , zed
    , hash ? lib.fakeHash
    , registry ? null
    , registryPath ? null
    , lockfilePath ? ".zpkg.lock"
    }:
    let
      registryValue =
        if registryPath != null then "file://${toString registryPath}"
        else registry;
      registryArgs = lib.optionalString (registryValue != null)
        "--registry ${lib.escapeShellArg registryValue}";
    in
    assert lib.assertMsg (registry == null || registryPath == null)
      "fetchZedArtifacts: set at most one of registry and registryPath";
    stdenvNoCC.mkDerivation {
      inherit pname version src;

      nativeBuildInputs = [ zed cacert coreutils findutils python3 ];
      phases = [ "installPhase" ];

      # A path-valued registry input creates the explicit sandbox dependency
      # required by a lock whose source is file:///nix/store/.... Callers that
      # already added the registry with `nix store add-path` should pass it as
      # `builtins.storePath <exact-path>` so the frozen URL identity is stable.
      ZED_PKG_NIX_REGISTRY_INPUT =
        if registryPath == null then "" else registryPath;

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
        export ZED_PKG_HOME="$TMPDIR/zed-home-must-remain-unused"
        export ZED_PKG_INTERACTIVE=0
        export ZED_PKG_ALLOW_BUILD=0
        export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
        export NIX_SSL_CERT_FILE="$SSL_CERT_FILE"

        # Private registry authentication is deliberately outside this first
        # reproducible bridge. Never inherit caller credentials into an FOD.
        unset \
          ZED_PKG_TOKEN \
          ZED_PKG_SUPABASE_KEY \
          ZED_PKG_AUTH_PASSWORD \
          ZED_PKG_FETCH_OUTPUT \
          ZED_PKG_FROZEN || true

        mkdir -p \
          "$HOME" \
          "$XDG_CACHE_HOME" \
          "$XDG_CONFIG_HOME" \
          "$XDG_DATA_HOME"

        ${lib.optionalString (registryPath != null) ''
          if [[ ! -d "$ZED_PKG_NIX_REGISTRY_INPUT" ]]; then
            echo "zed-pkg Nix artifact bridge: immutable registry input is missing" >&2
            exit 1
          fi
        ''}

        require_safe_relative() {
          local label="$1"
          local value="$2"
          case "$value" in
            ""|/*|.|..|../*|*/../*|*/..)
              echo "zed-pkg Nix artifact bridge: $label must be a safe relative path" >&2
              exit 1
              ;;
          esac
        }

        work="$TMPDIR/zed-project"
        mkdir -p "$work"
        lockfile_path=${lib.escapeShellArg lockfilePath}
        require_safe_relative lockfilePath "$lockfile_path"
        if [[ ! -f "$src/$lockfile_path" ]]; then
          echo "zed-pkg Nix artifact bridge: missing frozen lockfile" >&2
          exit 1
        fi
        cp "$src/$lockfile_path" "$work/.zpkg.lock"
        input_lock_digest="$(sha256sum "$work/.zpkg.lock" | cut -d' ' -f1)"

        bundle_parent="$TMPDIR/zed-fetch-output"
        mkdir -p "$bundle_parent"
        cd "$work"
        ${zed}/bin/zed ${registryArgs} fetch \
          --frozen \
          --output "$bundle_parent/bundle"

        bundle="$bundle_parent/bundle"
        test -d "$bundle/packages"
        test -f "$bundle/metadata/index.json"
        test -f "$bundle/metadata/lock.sha256"
        test -f "$bundle/metadata/zed-version.txt"
        test ! -e "$bundle/.zpkg.lock"
        test ! -e "$bundle/.zpkg.toml"
        test ! -e "$bundle/.zpkg-staging"
        test ! -e "$bundle/zed_modules"
        test ! -e "$bundle/node_modules"
        test ! -e "$ZED_PKG_HOME"

        expected_lock_line="$input_lock_digest  .zpkg.lock"
        actual_lock_line="$(cat "$bundle/metadata/lock.sha256")"
        if [[ "$actual_lock_line" != "$expected_lock_line" ]]; then
          echo "zed-pkg Nix artifact bridge: fetch bundle lock digest mismatch" >&2
          exit 1
        fi

        python3 - "$bundle" "$input_lock_digest" <<'PY'
        import json
        import pathlib
        import re
        import sys

        bundle = pathlib.Path(sys.argv[1])
        expected_lock_digest = sys.argv[2]
        index_path = bundle / "metadata" / "index.json"
        raw = index_path.read_text(encoding="utf-8")
        if "/nix/store/" in raw:
            raise SystemExit("fetch index retained a Nix store path")
        index = json.loads(raw)
        if index.get("schema") != "zed.fetch/v1":
            raise SystemExit("unexpected fetch bundle schema")
        if index.get("lock_sha256") != expected_lock_digest:
            raise SystemExit("fetch index retained the wrong lock digest")
        packages = index.get("packages")
        if not isinstance(packages, list):
            raise SystemExit("fetch package inventory must be an array")

        expected_order = sorted(
            packages,
            key=lambda package: (
                package.get("org", ""),
                package.get("name", ""),
                package.get("version", ""),
                package.get("sha256", ""),
            ),
        )
        if packages != expected_order:
            raise SystemExit("fetch package inventory is not canonical")

        allowed_source_kinds = {
            "file",
            "http",
            "https",
            "immutable-nix-store-input",
        }
        for position, package in enumerate(packages):
            if not isinstance(package, dict):
                raise SystemExit(f"package[{position}] must be an object")
            forbidden = {"source", "registry", "registry_url", "download_url"}
            if forbidden.intersection(package):
                raise SystemExit(f"package[{position}] retained a source literal")
            digest = package.get("sha256")
            if not isinstance(digest, str) or re.fullmatch(r"[0-9a-f]{64}", digest) is None:
                raise SystemExit(f"package[{position}] has an invalid artifact digest")
            relative = package.get("path")
            expected = f"packages/{digest}/pkg"
            if relative != expected:
                raise SystemExit(f"package[{position}] has a non-canonical payload path")
            if package.get("source_kind") not in allowed_source_kinds:
                raise SystemExit(f"package[{position}] has an unsupported source kind")
            payload = bundle / relative
            if not payload.is_dir():
                raise SystemExit(f"package[{position}] payload directory is missing")
            for value in package.values():
                if isinstance(value, str) and (
                    value.startswith("/") or "/nix/store/" in value
                ):
                    raise SystemExit(f"package[{position}] retained an absolute path")
        PY

        if find "$bundle" -type l -print -quit | grep -q .; then
          echo "zed-pkg Nix artifact bridge: fetch bundle contains a symlink" >&2
          exit 1
        fi

        mkdir -p "$out"
        cp -a "$bundle/." "$out/"

        runHook postInstall
      '';
    };
}
