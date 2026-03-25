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
        in
        {
          devShells.default = pkgs.mkShell {
            buildInputs = [
              schneeToolchain
              pkgs.llvmPackages.bintools-unwrapped # provides lld-link
              pkgs.llvmPackages.clang-unwrapped    # provides clang-cl
              pkgs.nix
            ];

            # Point cargo-schnee to the Windows SDK
            XWIN_DIR = "${winSdk}";

            # Tell cargo/rustc to use lld-link for the MSVC target
            CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = "lld-link";

            # For build scripts using the `cc` crate: forward CC/AR into derivations
            CARGO_SCHNEE_PASSTHRU_ENVS = "CC_x86_64_pc_windows_msvc AR_x86_64_pc_windows_msvc";
            CC_x86_64_pc_windows_msvc = "clang-cl";
            AR_x86_64_pc_windows_msvc = "llvm-lib";

            NIX_CONFIG = "extra-experimental-features = nix-command flakes pipe-operators ca-derivations dynamic-derivations recursive-nix";
          };
        };

    in
    {
      devShells = forAllSystems (system: (mkSystem system).devShells);
    };
}
