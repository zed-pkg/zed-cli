{ pkgs ? import <nixpkgs> { } }:

let
  installBridge = pkgs.callPackage ./zed-package.nix { };
  artifactBridge = pkgs.callPackage ./zed-fetch-bundle.nix { };
in
installBridge // artifactBridge
