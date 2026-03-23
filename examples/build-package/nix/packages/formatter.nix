{ pkgs, cargo-schnee, src }:

cargo-schnee.lib.buildPackage {
  inherit pkgs src;
  cargoLock = src + "/Cargo.lock";
  package = "formatter";
}
