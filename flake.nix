{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, treefmt-nix }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;

      mkSystem = system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs { inherit system overlays; };

          # Pin to a specific stable Rust that matches the cargo crate version
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
            targets = [ "aarch64-unknown-linux-gnu" ];
          };

          # Cross-compilation linker for aarch64
          crossCC = pkgs.pkgsCross.aarch64-multiplatform.stdenv.cc;

          # Build-only toolchain (no rust-src/rust-analyzer)
          rustBuild = pkgs.rust-bin.stable.latest.default;

          # Toolchain with clippy + rustfmt for CI checks
          rustCheck = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "clippy" "rustfmt" ];
          };

          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustBuild;
            rustc = rustBuild;
          };

          rustCheckPlatform = pkgs.makeRustPlatform {
            cargo = rustCheck;
            rustc = rustCheck;
          };

          treefmtEval = treefmt-nix.lib.evalModule pkgs ./treefmt.nix;

          cargo-schnee-dev = pkgs.writeShellScriptBin "cargo-schnee" ''
            exec cargo run --manifest-path "''${CARGO_SCHNEE_ROOT:-.}/Cargo.toml" -- "$@"
          '';

          # Only include files needed to build cargo-schnee, so that changes to
          # examples/ or other non-source files don't trigger a rebuild.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };

          commonBuildArgs = {
            pname = "cargo-schnee";
            version = "0.1.0";
            inherit src;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = with pkgs; [ pkg-config ];
            buildInputs = with pkgs; [ openssl curl libgit2 libssh2 zlib sqlite ];
            LIBGIT2_NO_VENDOR = "1";
            LIBSSH2_SYS_USE_PKG_CONFIG = "1";
          };

        in {
          formatter = treefmtEval.config.build.wrapper;

          packages.default = rustPlatform.buildRustPackage commonBuildArgs;

          checks = {
            build = self.packages.${system}.default;

            test = rustPlatform.buildRustPackage (commonBuildArgs // {
              doCheck = true;
            });

            clippy = rustCheckPlatform.buildRustPackage (commonBuildArgs // {
              pname = "cargo-schnee-clippy";
              nativeBuildInputs = commonBuildArgs.nativeBuildInputs ++ [ pkgs.clippy-sarif ];
              buildPhase = ''
                cargo clippy -- -D warnings
              '';
              installPhase = ''
                mkdir -p $out
              '';
              doCheck = false;
            });

            formatting = treefmtEval.config.build.check self;
          };

          devShells.default = pkgs.mkShell {
            buildInputs = with pkgs; [
              rustToolchain
              crossCC
              pkg-config
              openssl
              curl
              libgit2
              libssh2
              zlib
              sqlite
              nix # for nix CLI in the PoC
              cargo-schnee-dev
            ];

            # Needed for cargo crate's libgit2/openssl deps
            PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
            LIBGIT2_NO_VENDOR = "1";
            LIBSSH2_SYS_USE_PKG_CONFIG = "1";
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER = "${crossCC}/bin/aarch64-unknown-linux-gnu-gcc";

            # Required for dynamic derivation builds
            NIX_CONFIG = "experimental-features = nix-command flakes pipe-operators dynamic-derivations recursive-nix";

            shellHook = ''
              export CARGO_SCHNEE_ROOT="$PWD"
            '';
          };
        };

    in
    {
      lib.buildPackage = import ./nix/buildPackage.nix { inherit self; };
      lib.buildDoc = import ./nix/buildDoc.nix { inherit self; };
      lib.makeCargoWrapper = import ./nix/makeCargoWrapper.nix;

      lib.cargoOverrides = { pkgs }:
        let cargoSchnee = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
            schneeSetup = ''
              if [ -n "''${cargoDeps:-}" ]; then
                args+=(--vendor-dir "$cargoDeps")
              fi
              export HOME=''${HOME:-$(mktemp -d)}
            '';
        in {
          check = {
            command = "${cargoSchnee}/bin/cargo-schnee schnee check";
            forwardArgs = [ "--manifest-path" "--target" "--profile" "-p" "--package" "--features" ];
            boolArgs = [ "--no-default-features" ];
            setup = schneeSetup;
          };
          build = {
            command = "${cargoSchnee}/bin/cargo-schnee schnee build";
            forwardArgs = [ "--manifest-path" "--target" "--profile" "-p" "--package" "--features" "--bin" ];
            boolArgs = [ "--no-default-features" ];
            setup = schneeSetup;
            postRun = ''
              if [ -n "$__target" ]; then
                __pdir="debug"
                if [ -n "$__profile" ]; then
                  case "$__profile" in
                    dev) __pdir="debug" ;;
                    *) __pdir="$__profile" ;;
                  esac
                else
                  for _a in "''${args[@]}"; do
                    [ "$_a" = "--release" ] && __pdir="release"
                  done
                fi
                if [ -d "target/$__pdir" ] && [ ! -e "target/$__target/$__pdir" ]; then
                  mkdir -p "target/$__target"
                  ln -sfn "../$__pdir" "target/$__target/$__pdir"
                fi
              fi
            '';
          };
          run = {
            command = "${cargoSchnee}/bin/cargo-schnee schnee run";
            forwardArgs = [ "--manifest-path" "--target" "--profile" "-p" "--package" "--features" "--bin" ];
            boolArgs = [ "--no-default-features" ];
            setup = schneeSetup;
          };
          test = {
            command = "${cargoSchnee}/bin/cargo-schnee schnee test";
            forwardArgs = [ "--manifest-path" "--target" "--profile" "-p" "--package" "--features" ];
            boolArgs = [ "--no-default-features" ];
            setup = schneeSetup;
          };
          bench = {
            command = "${cargoSchnee}/bin/cargo-schnee schnee bench";
            forwardArgs = [ "--manifest-path" "--target" "--profile" "-p" "--package" "--features" ];
            boolArgs = [ "--no-default-features" ];
            setup = schneeSetup;
          };
          doc = {
            command = "${cargoSchnee}/bin/cargo-schnee schnee doc";
            forwardArgs = [ "--manifest-path" "--target" "--profile" "-p" "--package" "--features" ];
            boolArgs = [ "--no-default-features" "--no-deps" "--document-private-items" ];
            setup = schneeSetup;
          };
        };

      formatter = forAllSystems (system: (mkSystem system).formatter);
      packages = forAllSystems (system: (mkSystem system).packages);
      checks = forAllSystems (system: (mkSystem system).checks);
      devShells = forAllSystems (system: (mkSystem system).devShells);
    };
}
