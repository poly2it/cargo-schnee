{ pkgs, cargo-schnee, src }:

cargo-schnee.lib.testPackagePure {
  inherit pkgs src;
  cargoLock = src + "/Cargo.lock";
  package = "greeter";
}
