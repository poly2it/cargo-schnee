{
  description = "Example: Nix-packaged Rust project built with cargo-schnee";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    cargo-schnee.url = "path:../..";
  };

  outputs = { self, nixpkgs, rust-overlay, cargo-schnee }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;

      mkSystem = system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs { inherit system overlays; };
          inherit (pkgs) lib;

          rustToolchain = pkgs.rust-bin.stable.latest.default;
          schneeToolchain = cargo-schnee.lib.makeCargoWrapper {
            inherit pkgs rustToolchain;
            cargo = lib.getExe' rustToolchain "cargo";
            overrides = cargo-schnee.lib.cargoOverrides { inherit pkgs; };
          };

          rustPlatform = pkgs.makeRustPlatform {
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
          packages.default = rustPlatform.buildRustPackage {
            pname = "simple-project";
            version = "0.1.0";
            inherit src;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.nix ];

            # cargo-schnee invokes nix-store --realise internally,
            # which requires access to the Nix daemon from within the sandbox.
            requiredSystemFeatures = [ "recursive-nix" ];

            # Enable CA derivations (used by cargo-schnee's per-unit derivations)
            NIX_CONFIG = "extra-experimental-features = flakes pipe-operators ca-derivations";

            # Disable cargo-auditable: its wrapper inserts `auditable` before `build`,
            # bypassing our cargo wrapper's interception.
            auditable = false;

            # cargo-schnee handles testing differently
            doCheck = false;
          };

          # High-level API: cargo-schnee handles all wiring automatically
          packages.schnee = cargo-schnee.lib.buildPackage {
            inherit pkgs;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
              ];
            };
            cargoLock = ./Cargo.lock;
          };

          # Normal buildRustPackage for comparison (no cargo-schnee)
          packages.normal = pkgs.rustPlatform.buildRustPackage {
            pname = "simple-project";
            version = "0.1.0";
            inherit src;
            cargoLock.lockFile = ./Cargo.lock;
          };

          devShells.default = pkgs.mkShell {
            buildInputs = [
              schneeToolchain
              pkgs.nix
              pkgs.pkg-config
            ];
            NIX_CONFIG = "extra-experimental-features = nix-command flakes pipe-operators dynamic-derivations recursive-nix";
          };
        };

    in
    {
      packages = forAllSystems (system: (mkSystem system).packages);
      devShells = forAllSystems (system: (mkSystem system).devShells);
    };
}
