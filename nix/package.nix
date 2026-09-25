# steward.exe and stewardctl.exe. Called with a Windows package set -- the
# flake's `packages` use pkgsCross.mingwW64, and inside a winpkgs module `pkgs`
# already is one -- so the result is Windows binaries built on Linux.
{
  lib,
  stdenv,
  rustPlatform,
}:
rustPlatform.buildRustPackage {
  pname = "steward";
  inherit ((lib.importTOML ../Cargo.toml).workspace.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # Rust records source paths for panic messages, and the vendored crates live
  # in the store. winpkgs refuses to install a file that mentions /nix/store/,
  # so the paths are rewritten, and postInstall proves it.
  preBuild = ''
    export RUSTFLAGS="''${RUSTFLAGS-} --remap-path-prefix=/nix/store=/store --remap-path-prefix=$NIX_BUILD_TOP=/build"
  '';

  # The tests are steward-unit's, and run natively (checks.unit); a cross build
  # cannot run Windows test binaries.
  doCheck = false;

  postInstall = ''
    if grep -l --binary-files=text /nix/store/ $out/bin/*; then
      echo "the binaries above mention /nix/store/; winpkgs would refuse them" >&2
      exit 1
    fi
  '';

  meta = {
    description = "A per-user service manager for Windows";
    homepage = "https://github.com/calebstewart/steward";
    license = lib.licenses.mit;
    platforms = lib.platforms.windows;
    mainProgram = "stewardctl";
  };
}
