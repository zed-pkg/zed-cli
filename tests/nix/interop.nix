{ pkgs ? import <nixpkgs> { }
, zedBinary
, consumerSrc
, registrySrc
, hash ? pkgs.lib.fakeHash
, name ? "zed-nix-deps"
, mode ? "deps"
}:

let
  bridge = pkgs.callPackage ../../nix/zed-package.nix { };

  # The workflow builds the exact branch binary first, then imports it as an
  # immutable Nix path. This keeps the interop test focused on zed-pkg's
  # dependency boundary rather than on a second Rust packaging implementation.
  #
  # A hosted Linux runner binary names the host ELF interpreter and runtime
  # libraries. Normalize those references inside Nix so execution proves the
  # sandbox contract rather than accidentally depending on /lib from the host.
  zed = pkgs.stdenvNoCC.mkDerivation {
    pname = "zed-under-test";
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

  zedDeps = bridge.fetchZedDeps {
    pname = name;
    version = "1.0.0";
    src = consumerSrc;
    inherit zed hash;
    registry = "file://${toString registrySrc}";
    adapter = "node";
  };

  consumer = bridge.mkZedPackage {
    pname = "${name}-consumer";
    version = "1.0.0";
    src = consumerSrc;
    inherit zedDeps;

    nativeBuildInputs = [ pkgs.nodejs ];
    dontBuild = true;
    doCheck = true;

    checkPhase = ''
      runHook preCheck
      node src/main.js
      test -f .vendor/.zed/zed-pkg/docker-node-lib/package.json
      test -f node_modules/@zed-pkg/docker-node-lib/package.json
      runHook postCheck
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p "$out/share/zed-nix-consumer"
      cp -a . "$out/share/zed-nix-consumer/"
      runHook postInstall
    '';
  };
in
if mode == "deps" then zedDeps
else if mode == "package" then consumer
else throw "tests/nix/interop.nix: mode must be 'deps' or 'package'"
