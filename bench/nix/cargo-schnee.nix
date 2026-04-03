# cargo-schnee — per-compilation-unit content-addressed derivations via
# recursive-nix.  On incremental builds only actually-changed compilation
# units are rebuilt; unchanged units are found in the store by their CA hash.
{ pkgs, cargo-schnee, justSrc, justSrcModified, rustToolchain }:

let
  # Use the SAME cargoLock for both builds.  The Cargo.lock content is
  # identical (only src/main.rs changed), so pointing both at justSrc's
  # lockfile ensures the vendored-deps derivation hash is the same.
  # This gives cargo-schnee the same vendor store path for both builds,
  # which is required for per-unit CA derivation deduplication to work:
  # unchanged dep crate derivations get the same .drv hash and are
  # skipped on the incremental build.
  sharedCargoLock = justSrc + "/Cargo.lock";

  # Pre-built vendor directory.  Using cargoDeps instead of cargoLock avoids
  # nixpkgs creating its own FOD (same content, different hash).
  cargoVendoredDeps = (pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  }).importCargoLock {
    lockFile = sharedCargoLock;
  };

  common = {
    inherit pkgs rustToolchain;
    doCheck = false;
    cargoDeps = cargoVendoredDeps;
  };

in {
  clean = cargo-schnee.lib.buildPackage (common // {
    src = justSrc;
  });

  incremental = cargo-schnee.lib.buildPackage (common // {
    src = justSrcModified;
  });
}
