{ pkgs, cargo-schnee, src }:

cargo-schnee.lib.buildPackagePure {
  inherit pkgs src;
  cargoDeps = pkgs.rustPlatform.importCargoLock {
    lockFile = src + "/Cargo.lock";
  };
  package = "greeter";
  name = "greeter-pure";
}
