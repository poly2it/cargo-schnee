{
  description = "Example: cross-compiling a Rust project to x86_64-pc-windows-msvc with cargo-schnee";

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
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;

      mkSystem = system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs {
            inherit system overlays;
            config = {
              allowUnfree = true;
              microsoftVisualStudioLicenseAccepted = true;
            };
          };
          inherit (pkgs) lib;

          # Rust toolchain with MSVC target support
          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            targets = [ "x86_64-pc-windows-msvc" ];
          };

          schneeToolchain = cargo-schnee.lib.makeCargoWrapper {
            inherit pkgs rustToolchain;
            cargo = lib.getExe' rustToolchain "cargo";
            overrides = cargo-schnee.lib.cargoOverrides { inherit pkgs; };
          };

          # Windows SDK (MSVC CRT + Windows SDK headers/libs)
          # Requires: config.microsoftVisualStudioLicenseAccepted = true
          winSdk = pkgs.windows.sdk;

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
            pname = "cross-windows-example";
            version = "0.1.0";
            inherit src;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [
              pkgs.nix
              pkgs.llvmPackages.bintools-unwrapped # lld-link
              pkgs.llvmPackages.clang-unwrapped    # clang-cl
              pkgs.wineWow64Packages.stable       # run tests via Wine
            ];

            requiredSystemFeatures = [ "recursive-nix" ];
            NIX_CONFIG = "extra-experimental-features = flakes pipe-operators ca-derivations";

            # Cross-compilation environment
            XWIN_DIR = "${winSdk}";
            CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = "lld-link";
            CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER = "wine";

            # Run Wine headless: suppress Mono/Gecko installer dialogs
            WINEDLLOVERRIDES = "mscoree=d;mshtml=d";
            DISPLAY = "";

            # Disable cargo-auditable: its wrapper bypasses our cargo wrapper.
            auditable = false;

            # Override build/install/check phases: there is no pkgsCross.msvc
            # stdenv, so we pass --target manually. cargo on PATH is the schnee
            # wrapper from rustPlatform, which intercepts cargo subcommands and
            # runs cargo-schnee instead.
            buildPhase = ''
              runHook preBuild
              cargo build --release --target x86_64-pc-windows-msvc
              runHook postBuild
            '';

            checkPhase = ''
              runHook preCheck
              # Wine needs a writable HOME to create its WINEPREFIX (~/.wine).
              # The sandbox sets HOME=/homeless-shelter which doesn't exist.
              export HOME="$TMPDIR/wine-home"
              mkdir -p "$HOME"
              # Fully headless: disconnect from both X11 and Wayland.
              unset WAYLAND_DISPLAY
              cargo test --release --target x86_64-pc-windows-msvc
              runHook postCheck
            '';

            installPhase = ''
              runHook preInstall
              mkdir -p $out/bin
              cp target/x86_64-pc-windows-msvc/release/*.exe $out/bin/
              cp target/x86_64-pc-windows-msvc/release/*.pdb $out/bin/ 2>/dev/null || true
              runHook postInstall
            '';

            # Skip fixup: patchelf/strip don't work on PE binaries
            dontFixup = true;

            doCheck = true;
          };

          devShells.default = pkgs.mkShell {
            buildInputs = [
              schneeToolchain
              pkgs.llvmPackages.bintools-unwrapped # provides lld-link
              pkgs.llvmPackages.clang-unwrapped    # provides clang-cl
              pkgs.wineWow64Packages.stable       # run .exe via Wine
              pkgs.nix
            ];

            # Point cargo-schnee to the Windows SDK
            XWIN_DIR = "${winSdk}";

            # Tell cargo/rustc to use lld-link for the MSVC target
            CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = "lld-link";

            # Run cross-compiled Windows binaries via Wine (headless)
            CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER = "wine";
            WINEDLLOVERRIDES = "mscoree=d;mshtml=d";

            # For build scripts using the `cc` crate: forward CC/AR into derivations
            CARGO_SCHNEE_PASSTHRU_ENVS = "CC_x86_64_pc_windows_msvc AR_x86_64_pc_windows_msvc";
            CC_x86_64_pc_windows_msvc = "clang-cl";
            AR_x86_64_pc_windows_msvc = "llvm-lib";

            NIX_CONFIG = "extra-experimental-features = nix-command flakes pipe-operators ca-derivations dynamic-derivations recursive-nix";
          };
        };

    in
    {
      packages = forAllSystems (system: (mkSystem system).packages);
      devShells = forAllSystems (system: (mkSystem system).devShells);
    };
}
