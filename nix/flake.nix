{
  description = "Reproducible Nix bridge for the independent zed-pkg package manager";

  inputs = {
    # Keep this lock aligned with the reviewed zed-pkg Nix baseline. Consumers
    # still pin this flake independently in their own flake.lock.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      lib = {
        makeZedPackageLib = pkgs:
          pkgs.callPackage ./zed-package.nix { };

        # CI uses the exact locked input as NIX_PATH for the legacy expression
        # canary; it never falls back to a mutable channel lookup.
        nixpkgsPath = nixpkgs.outPath;
      };

      # Keep the reusable library's fail-closed argument contract executable.
      # The existing Linux/macOS interop workflow invokes `nix flake check
      # --no-build`, so these checks instantiate derivations without executing
      # the dummy CLI or duplicating the full fixed-output canary.
      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          evaluation-contract = import ./tests/evaluation-contract.nix {
            inherit pkgs;
          };
        }
      );
    };
}
