{ pkgs ? import <nixpkgs> { } }:

pkgs.callPackage ./zed-package.nix { }
