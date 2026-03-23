{
  description = "Example: lib.buildPackage, the high-level cargo-schnee API";

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

          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
            ];
          };

          callPackage = pkgs.lib.callPackageWith {
            inherit pkgs cargo-schnee src;
          };
        in
        {
          packages.greeter = callPackage ./nix/packages/greeter.nix {};
          packages.formatter = callPackage ./nix/packages/formatter.nix {};
          packages.default = self.packages.${system}.greeter;
        };

    in
    {
      packages = forAllSystems (system: (mkSystem system).packages);
    };
}
