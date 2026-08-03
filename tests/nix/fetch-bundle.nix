{ pkgs ? import <nixpkgs> { }
, zedBinary
, consumerSrc
, registryStore ? null
, hash ? pkgs.lib.fakeHash
, name ? "zed-nix-artifacts"
}:

let
  bridge = import ../../nix/default.nix { inherit pkgs; };

  registryInput =
    if registryStore == null then null
    else builtins.storePath registryStore;

  # The workflow builds the exact branch binary first and registers it with
  # Nix. Linux runner binaries are normalized in a Nix derivation so the
  # sandbox never depends on the host's /lib interpreter or runtime closure.
  zed = pkgs.stdenvNoCC.mkDerivation {
    pname = "zed-fetch-under-test";
    version = "0";
    dontUnpack = true;
    strictDeps = true;

    nativeBuildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [
      pkgs.autoPatchelfHook
    ];
    buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [
      pkgs.glibc
      pkgs.stdenv.cc.cc.lib
    ];

    installPhase = ''
      runHook preInstall
      mkdir -p "$out/bin"
      cp ${zedBinary} "$out/bin/zed"
      chmod 0755 "$out/bin/zed"
      runHook postInstall
    '';
  };
in
bridge.fetchZedArtifacts {
  pname = name;
  version = "1.0.0";
  src = consumerSrc;
  inherit zed hash;
  registryPath = registryInput;
}
