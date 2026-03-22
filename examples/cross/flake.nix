{
  description = "Example: cross-compiling a Rust project to aarch64 with cargo-schnee";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    cargo-schnee.url = "path:../..";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, cargo-schnee }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        crossPkgs = pkgs.pkgsCross.aarch64-multiplatform;
        inherit (pkgs) lib;

        # Rust toolchain with aarch64 target support (runs on build machine)
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "aarch64-unknown-linux-gnu" ];
        };

        schneeToolchain = cargo-schnee.lib.makeCargoWrapper {
          inherit pkgs rustToolchain;
          cargo = lib.getExe' rustToolchain "cargo";
          overrides = cargo-schnee.lib.cargoOverrides { inherit pkgs; };
        };

        # Use cross pkgs' makeRustPlatform so stdenv.hostPlatform is aarch64.
        # The build hook will automatically pass --target aarch64-unknown-linux-gnu.
        # cargo/rustc are build-time tools that run on the build machine (x86_64).
        crossRustPlatform = crossPkgs.makeRustPlatform {
          cargo = schneeToolchain;
          rustc = schneeToolchain;
        };

        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
          ];
        };
      in
      {
        packages.default = crossRustPlatform.buildRustPackage {
          pname = "cross-example";
          version = "0.1.0";
          inherit src;
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [ pkgs.nix ];

          requiredSystemFeatures = [ "recursive-nix" ];
          NIX_CONFIG = "extra-experimental-features = flakes pipe-operators ca-derivations";

          # Disable cargo-auditable: its wrapper inserts `auditable` before `build`,
          # bypassing our cargo wrapper's interception.
          auditable = false;

          doCheck = false;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            schneeToolchain
            crossPkgs.stdenv.cc
            pkgs.nix
            pkgs.pkg-config
          ];
          CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = "${crossPkgs.stdenv.cc}/bin/aarch64-unknown-linux-gnu-gcc";
          NIX_CONFIG = "extra-experimental-features = nix-command flakes pipe-operators dynamic-derivations recursive-nix";
        };
      }
    );
}
