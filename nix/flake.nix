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
        # Expose both the install-shaped and resolver-only bridge families from
        # one pinned library without collapsing their distinct output contracts.
        makeZedPackageLib = pkgs:
          import ./default.nix { inherit pkgs; };

        # CI uses the exact locked input as NIX_PATH for the legacy expression
        # canary; it never falls back to a mutable channel lookup.
        nixpkgsPath = nixpkgs.outPath;
      };

      # Ratchet the reusable public argument boundary without executing either
      # bridge. Integration canaries remain the authority for fixed-output
      # realization, recursive hashes, tamper rejection, and offline consumers.
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
