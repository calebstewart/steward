{
  description = "steward - a per-user service manager for Windows";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Only for the checks that evaluate the modules below; a consumer brings
    # its own winpkgs.
    winpkgs = {
      url = "github:calebstewart/winpkgs";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      winpkgs,
    }:
    let
      inherit (nixpkgs) lib;
      # Systems that build; what they build is always Windows.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      # The documentation site, built for the build machine rather than for
      # Windows. Not in the overlay: nobody installs a website.
      mkDocs = pkgs: pkgs.callPackage ./nix/docs.nix { };
    in
    {
      packages = forAllSystems (pkgs: {
        steward = pkgs.pkgsCross.mingwW64.callPackage ./nix/package.nix { };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.steward;
        docs = mkDocs pkgs;
      });

      # For a package set that already targets Windows, such as `pkgs` inside a
      # winpkgs module.
      overlays.default = final: _prev: {
        steward = final.callPackage ./nix/package.nix { };
      };

      # winpkgs modules, under winpkgs' own name for its module trees:
      # `system` installs steward and registers the per-user service template,
      # `home` writes a user's units from home-manager's systemd.user.services.
      windowsModules = {
        system = ./nix/winpkgs/system.nix;
        home = ./nix/winpkgs/home.nix;
      };

      checks = forAllSystems (
        pkgs:
        {
          unit = pkgs.callPackage ./nix/unit-tests.nix { };
          windows = self.packages.${pkgs.stdenv.hostPlatform.system}.steward;
          # A broken template or a dead `@/` link fails the build, so the site
          # cannot go stale unnoticed.
          docs = mkDocs pkgs;
        }
        // import ./nix/winpkgs/checks.nix {
          inherit pkgs self winpkgs;
        }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
