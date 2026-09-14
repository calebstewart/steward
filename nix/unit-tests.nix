# The tests of the crates with no Windows in them -- the unit parser, the
# supervisor's state machine, the control protocol, the Event Log names
# and GUIDs the provisioning and the shim have to agree on, and what the
# shim holds and writes -- run natively.
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
    "steward-eventlog"
    "-p"
    "steward-supervisor"
    "-p"
    "steward-ipc"
    "-p"
    "steward-cat"
  ];
  cargoTestFlags = [
    "-p"
    "steward-unit"
    "-p"
    "steward-eventlog"
    "-p"
    "steward-supervisor"
    "-p"
    "steward-ipc"
    "-p"
    "steward-cat"
  ];

  # A library has nothing to install; the check passing is the product.
  installPhase = ''
    runHook preInstall
    touch $out
    runHook postInstall
  '';
}
