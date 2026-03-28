{
  description = "Example: cross-compiling to Windows with lib.buildPackage";

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

          rustToolchain = pkgs.rust-bin.stable.latest.default.override {
            targets = [ "x86_64-pc-windows-msvc" ];
          };

          winSdk = pkgs.windows.sdk;

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
          packages.default = cargo-schnee.lib.buildPackage {
            inherit pkgs src rustToolchain;
            target = "x86_64-pc-windows-msvc";
            cargoLock = ./Cargo.lock;

            nativeBuildInputs = [
              pkgs.llvmPackages.bintools-unwrapped
              pkgs.llvmPackages.clang-unwrapped
            ];

            env = {
              XWIN_DIR = "${winSdk}";
              CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = "lld-link";
            };
          };
        };

    in
    {
      packages = forAllSystems (system: (mkSystem system).packages);
    };
}
