# cargo2nix — per-crate derivation build.  Best-effort: if Cargo.nix
# generation or the build itself fails the benchmark records SKIPPED.
#
# cargo2nix works by:
#   1. Generating a Cargo.nix file from Cargo.lock (IFD)
#   2. Building each crate as a separate Nix derivation
# Unchanged crates keep the same derivation hash on incremental builds.
{ pkgs, cargo2nix, justSrc, justSrcModified, cargoVendoredDeps, rustToolchain, system }:

let
  # Apply cargo2nix overlay so pkgs.rustBuilder is available.
  c2nPkgs = import pkgs.path {
    inherit system;
    overlays = [
      cargo2nix.overlays.default
    ];
  };

  # Generate Cargo.nix via IFD (import from derivation).
  # This runs the cargo2nix tool on the Just source to produce a Nix
  # expression describing per-crate derivations.
  generateCargoNix = src:
    pkgs.runCommand "just-cargo2nix-generated" {
      nativeBuildInputs = [
        cargo2nix.packages.${system}.cargo2nix
        rustToolchain
        pkgs.git
      ];
    } ''
      cp -r ${src} source
      chmod -R u+w source
      cd source

      # cargo2nix needs a git repo for version detection
      git init -q
      git add -A
      git -c user.name=bench -c user.email=bench@localhost commit -q -m "init" --allow-empty

      # Set up vendored deps so cargo2nix works without network
      mkdir -p .cargo
      cat > .cargo/config.toml <<CARGO_EOF
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "${cargoVendoredDeps}"
CARGO_EOF

      cargo2nix -o -l
      cp Cargo.nix $out
    '';

  mkBuild = src:
    let
      cargoNixFile = generateCargoNix src;

      # Use cargo2nix's overlay API to build the generated Cargo.nix.
      rustPkgs = c2nPkgs.rustBuilder.makePackageSet {
        packageFun = import cargoNixFile;
        rustToolchain = rustToolchain;
        workspaceSrc = src;
        ignoreLockHash = true;
      };
    in
      # The workspace member "just" contains the binary.
      (rustPkgs.workspace.just {});

in {
  clean = mkBuild justSrc;
  incremental = mkBuild justSrcModified;
}
