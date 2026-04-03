# Fetches Just 1.40.0 source and produces a modified variant for incremental
# build testing.  The modification appends a blank line to src/lib.rs — enough
# to change the source hash and trigger recompilation of the library crate
# (all ~90 modules) and the binary crate, while all vendored dependency
# crates remain unchanged.
{ pkgs, rustPlatform }:

let
  justSrc = pkgs.fetchFromGitHub {
    owner = "casey";
    repo = "just";
    rev = "1.40.0";
    hash = "sha256-pmuwZoBIgUsKWFTXo8HYHVxrDWPMO8cumD/UHajFS6A=";
  };

  # Append a newline to src/lib.rs and src/main.rs.  This triggers
  # recompilation of both the library and binary crates, while all ~117
  # vendored dependency crates stay cached — the most meaningful incremental
  # scenario for a workspace-style project.
  justSrcModified = pkgs.runCommand "just-src-modified" { } ''
    cp -r ${justSrc} $out
    chmod -R u+w $out
    echo "" >> $out/src/lib.rs
    echo "" >> $out/src/main.rs
  '';

  # Vendored dependencies for the imperative cargo-build baseline.  All
  # Nix-based build systems vendor internally via cargoLock.lockFile.
  cargoVendoredDeps = rustPlatform.importCargoLock {
    lockFile = justSrc + "/Cargo.lock";
  };

in {
  inherit justSrc justSrcModified cargoVendoredDeps;
}
