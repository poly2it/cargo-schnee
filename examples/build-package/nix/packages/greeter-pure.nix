{ pkgs, cargo-schnee, src }:

cargo-schnee.lib.buildPackagePure {
  inherit pkgs src;
  cargoLock = src + "/Cargo.lock";
  package = "greeter";
}
