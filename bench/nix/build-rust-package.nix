# Standard nixpkgs buildRustPackage — monolithic derivation.
# Any source change produces an entirely new derivation hash, so the
# incremental build is effectively a full rebuild.
{ pkgs, rustPlatform, justSrc, justSrcModified, cargoVendoredDeps }:

let
  common = {
    pname = "just";
    version = "1.40.0";
    doCheck = false;
    # Use pre-vendored deps so the vendor directory is a known store path
    # (avoids an internal FOD that unsafeDiscardStringContext would skip).
    cargoDeps = cargoVendoredDeps;
    meta.description = "Just benchmark build (buildRustPackage)";
  };

in {
  clean = rustPlatform.buildRustPackage (common // {
    src = justSrc;
  });

  incremental = rustPlatform.buildRustPackage (common // {
    src = justSrcModified;
  });
}
