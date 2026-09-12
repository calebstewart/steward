# The tests of the crates with no Windows in them -- the unit parser and the
# supervisor's state machine -- run natively.
{ lib, rustPlatform }:
rustPlatform.buildRustPackage {
  pname = "steward-native-tests";
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
    "-p"
    "steward-supervisor"
  ];
  cargoTestFlags = [
    "-p"
    "steward-unit"
    "-p"
    "steward-supervisor"
  ];

  # A library has nothing to install; the check passing is the product.
  installPhase = ''
    runHook preInstall
    touch $out
    runHook postInstall
  '';
}
