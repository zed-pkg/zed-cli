{
  description = "Closure-free Nix output for DEN-1419 sealing canaries";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/e73de5be04e0eff4190a1432b946d469c794e7b4";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          portable = pkgs.runCommandNoCC "zed-nix-portable-1.0.0" { } ''
            mkdir -p "$out/bin" "$out/share/zed-nix-portable"
            printf '%s\n' \
              '#!/bin/sh' \
              "printf '%s\\n' 'portable Nix output sealed for Zed'" \
              > "$out/bin/zed-nix-portable"
            chmod 0755 "$out/bin/zed-nix-portable"
            printf '%s\n' 'closure-free fixture data' \
              > "$out/share/zed-nix-portable/message.txt"
          '';
        in
        {
          inherit portable;
          default = portable;
        }
      );
    };
}
