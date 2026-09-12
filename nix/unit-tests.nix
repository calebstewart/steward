# steward-unit's tests, natively: the parser is plain Rust and the only crate
# whose tests mean anything off Windows.
{ lib, rustPlatform }:
rustPlatform.buildRustPackage {
  pname = "steward-unit-tests";
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

  cargoBuildFlags = [
    "-p"
    "steward-unit"
  ];
  cargoTestFlags = [
    "-p"
    "steward-unit"
  ];

  # A library has nothing to install; the check passing is the product.
  installPhase = ''
    runHook preInstall
    touch $out
    runHook postInstall
  '';
}
