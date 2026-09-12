# The tests of the crates with no Windows in them -- the unit parser, the
# supervisor's state machine, the control protocol -- run natively.
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
      # The tests check that the example units still load.
      ../examples
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  cargoBuildFlags = [
    "-p"
    "steward-unit"
    "-p"
    "steward-supervisor"
    "-p"
    "steward-ipc"
  ];
  cargoTestFlags = [
    "-p"
    "steward-unit"
    "-p"
    "steward-supervisor"
    "-p"
    "steward-ipc"
  ];

  # A library has nothing to install; the check passing is the product.
  installPhase = ''
    runHook preInstall
    touch $out
    runHook postInstall
  '';
}
