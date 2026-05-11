# cargo-schnee — per-compilation-unit content-addressed derivations via
# recursive-nix.  On incremental builds only actually-changed compilation
# units are rebuilt; unchanged units are found in the store by their CA hash.
{ pkgs, cargo-schnee, justSrc, justSrcModified, rustToolchain }:

let
  # Use the SAME cargoLock for both builds.  The Cargo.lock content is
  # identical (only src/main.rs changed), so pointing both at justSrc's
  # lockfile ensures the vendored-deps derivation hash is the same.
  # This gives cargo-schnee the same vendor store path for both builds,
  # which is required for per-unit CA derivation deduplication to work:
  # unchanged dep crate derivations get the same .drv hash and are
  # skipped on the incremental build.
  sharedCargoLock = justSrc + "/Cargo.lock";

  # Pre-built vendor directory.  Using cargoDeps instead of cargoLock avoids
  # nixpkgs creating its own FOD (same content, different hash).
  cargoVendoredDeps = (pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  }).importCargoLock {
    lockFile = sharedCargoLock;
  };

  common = {
    inherit pkgs rustToolchain;
    doCheck = false;
    cargoDeps = cargoVendoredDeps;
  };

  # Pre-compute the cargo unit graph once and feed it to both variants
  # via `CARGO_SCHNEE_UNIT_GRAPH`. The graph is a deterministic function
  # of cargo's resolution inputs (Cargo.lock + workspace Cargo.toml
  # files + cargoDeps + cargo-schnee version + profile/target/etc.).
  # `justSrc` and `justSrcModified` share Cargo.lock and the same
  # Cargo.toml content — only `src/lib.rs` / `src/main.rs` differ — so
  # one graph file serves both.
  unitGraphDrv = cargo-schnee.lib.unitGraph {
    inherit pkgs rustToolchain;
    src = justSrc;
    cargoDeps = cargoVendoredDeps;
  };

  # Capture cargo-schnee's planner spans into a Chrome `trace_event`
  # file under the VM's 9p-shared `/results` mount, one file per
  # variant. The host runner copies these into `bench/profiles/`
  # alongside the existing nixprof traces — see the matching `cp` glob
  # in `bench/flake.nix`.
  #
  # `bench/nix/vm.nix` chmods `/results` 1777 at boot so the build user
  # (`nixbld`) can write through the mount; without that, `init_tracing`
  # in cargo-schnee logs a warning and silently disables the chrome
  # layer rather than crashing the binary.
  schneeBenchEnv = name: {
    CARGO_SCHNEE_TRACE = "/results/schnee-planner-${name}.trace_event";
    CARGO_SCHNEE_UNIT_GRAPH = "${unitGraphDrv}";
  };

in {
  clean = cargo-schnee.lib.buildPackage (common // {
    src = justSrc;
    env = schneeBenchEnv "clean";
  });

  incremental = cargo-schnee.lib.buildPackage (common // {
    src = justSrcModified;
    env = schneeBenchEnv "incremental";
  });
}
