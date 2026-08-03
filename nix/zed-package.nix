{ lib, stdenv, stdenvNoCC, cacert, coreutils, findutils }:

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

  shellArgs = values:
    lib.concatStringsSep " " (map lib.escapeShellArg values);
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
    , manifestPath ? ".zpkg.toml"
    , lockfilePath ? ".zpkg.lock"
    , adapter ? "none"
    , target ? null
    , extraArgs ? [ ]
    }:
    let
      registryArgs = lib.optionalString (registry != null)
        "--registry ${lib.escapeShellArg registry}";
      targetArgs = lib.optionalString (target != null)
        "--target ${lib.escapeShellArg target}";
      manifestArgs = lib.optionalString (manifestPath == null) "--skip-manifest";
      extraArgsString = shellArgs extraArgs;
      contract = builtins.toJSON {
        schema_version = 1;
        resolver = "zed-pkg";
        lockfile = ".zpkg.lock";
        install_mode = "copy";
        inherit adapter target;
        build_hooks = false;
        registry_override = registry;
      };
    in
    stdenvNoCC.mkDerivation {
      inherit pname version src;

      nativeBuildInputs = [ zed cacert coreutils findutils ];
      phases = [ "installPhase" ];

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

        work="$TMPDIR/zed-project"
        mkdir -p "$work"

        lockfile_path=${lib.escapeShellArg lockfilePath}
        if [[ ! -f "$src/$lockfile_path" ]]; then
          echo "zed-pkg Nix bridge: missing lockfile $lockfile_path in $src" >&2
          exit 1
        fi
        cp "$src/$lockfile_path" "$work/.zpkg.lock"

        ${lib.optionalString (manifestPath != null) ''
          manifest_path=${lib.escapeShellArg manifestPath}
          if [[ ! -f "$src/$manifest_path" ]]; then
            echo "zed-pkg Nix bridge: missing manifest $manifest_path in $src" >&2
            exit 1
          fi
          cp "$src/$manifest_path" "$work/.zpkg.toml"
        ''}

        cd "$work"
        ${zed}/bin/zed ${registryArgs} install \
          --frozen \
          --install-mode copy \
          --adapter ${lib.escapeShellArg adapter} \
          ${targetArgs} \
          ${manifestArgs} \
          ${extraArgsString}

        if [[ -e "$work/.zpkg-staging" ]]; then
          echo "zed-pkg Nix bridge: successful install left transaction state behind" >&2
          exit 1
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

        cp "$work/.zpkg.lock" "$out/metadata/.zpkg.lock"
        ${lib.optionalString (manifestPath != null) ''
          cp "$work/.zpkg.toml" "$out/metadata/.zpkg.toml"
        ''}
        printf '%s\n' ${lib.escapeShellArg contract} > "$out/metadata/contract.json"
        lock_digest="$(sha256sum "$out/metadata/.zpkg.lock" | cut -d' ' -f1)"
        printf '%s  %s\n' "$lock_digest" .zpkg.lock > "$out/metadata/lock.sha256"
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
    args@{ zedDeps, postPatch ? "", passthru ? { }, ... }:
    stdenv.mkDerivation (
      (builtins.removeAttrs args [ "zedDeps" "postPatch" "passthru" ])
      // {
        ZED_PKG_DEPS = zedDeps;
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
