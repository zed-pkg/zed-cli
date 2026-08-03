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
  zed = pkgs.runCommand "zed-under-test" { } ''
    mkdir -p "$out/bin"
    cp ${zedBinary} "$out/bin/zed"
    chmod 0555 "$out/bin/zed"
  '';

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
