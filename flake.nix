{
  description = "steward - a per-user service manager for Windows";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      # Systems that build; what they build is always Windows.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        steward = pkgs.pkgsCross.mingwW64.callPackage ./nix/package.nix { };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.steward;
      });

      # For a package set that already targets Windows, such as `pkgs` inside a
      # winpkgs module.
      overlays.default = final: _prev: {
        steward = final.callPackage ./nix/package.nix { };
      };

      checks = forAllSystems (pkgs: {
        unit = pkgs.callPackage ./nix/unit-tests.nix { };
        windows = self.packages.${pkgs.stdenv.hostPlatform.system}.steward;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
