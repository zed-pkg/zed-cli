{
  description = "Reproducible Nix bridge for the independent zed-pkg package manager";

  outputs = { self }:
    {
      lib.makeZedPackageLib = pkgs:
        pkgs.callPackage ./zed-package.nix { };
    };
}
