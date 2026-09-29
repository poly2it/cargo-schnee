{ pkgs, cargo-schnee, src }:

cargo-schnee.lib.testPackage {
  inherit pkgs src;
  cargoLock = src + "/Cargo.lock";
  package = "greeter";
}
