{ pkgs, cargo-schnee, src }:

cargo-schnee.lib.buildDoc {
  inherit pkgs src;
  cargoLock = src + "/Cargo.lock";
  documentPrivateItems = true;
}
