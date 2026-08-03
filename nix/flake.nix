{
  description = "Reproducible Nix bridge for the independent zed-pkg package manager";

  inputs = {
    # Keep this lock aligned with the reviewed zed-pkg Nix baseline. Consumers
    # still pin this flake independently in their own flake.lock.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    {
      lib = {
        makeZedPackageLib = pkgs:
          pkgs.callPackage ./zed-package.nix { };

        # CI uses the exact locked input as NIX_PATH for the legacy expression
        # canary; it never falls back to a mutable channel lookup.
        nixpkgsPath = nixpkgs.outPath;
      };
    };
}
