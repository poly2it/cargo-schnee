# nix/unitGraph.nix — pre-compute the cargo-schnee unit graph as a
# content-addressed derivation.
#
# Output is a single store path containing `graph.json`: the serialised
# unit-graph cache entry that downstream cargo-schnee invocations consume
# via `CARGO_SCHNEE_UNIT_GRAPH`. cargo-schnee validates the embedded cache
# key on load and falls back to a full bootstrap if anything mismatches,
# so a stale or wrong-input graph is silently ignored rather than served.
#
# The cache key is a deterministic function of cargo's resolver inputs:
# Cargo.lock, every workspace `Cargo.toml`, the vendored crate sources,
# the active profile, target triple, intent, package selection, and
# feature set. Any change invalidates the derivation; identical inputs
# share the same store path across builds, branches, and machines.
#
# This helper is exposed as `self.lib.unitGraph`. Wire it into a
# downstream `buildPackage` invocation by passing the realised path as
# `CARGO_SCHNEE_UNIT_GRAPH`:
#
#   buildPackage = self.lib.buildPackage;
#   unitGraph    = self.lib.unitGraph;
#   gp = unitGraph { inherit pkgs src cargoDeps rustToolchain
#                            buildType target features; };
#   pkg = buildPackage {
#     inherit pkgs src cargoDeps rustToolchain;
#     env = { CARGO_SCHNEE_UNIT_GRAPH = "${gp}"; };
#   };
#
# `buildPackage` does not call `unitGraph` automatically because the
# resolver-input declaration belongs to the consumer — features and
# package selection that today live inside `cargoExtraArgs` would have
# to be lifted out, and we want callers to make the trade-off
# explicitly until the wider integration is settled.
{ self }:

{
  pkgs,
  src,
  # The vendored crate-source derivation. Required: cargo's resolver
  # reads each vendored crate's Cargo.toml during resolution with
  # `--offline --frozen`.
  cargoDeps,
  rustToolchain ? null,
  # `dev`, `release`, or any custom profile name declared in the
  # workspace's [profile] tables.
  buildType ? "release",
  # Cross-compilation target triple, e.g. `x86_64-pc-windows-msvc`.
  # `null` means host platform.
  target ? null,
  # Mirrors `cargo --features`; one entry per `--features` invocation.
  features ? [],
  # Mirrors `cargo --no-default-features`.
  noDefaultFeatures ? false,
  # `cargo -p <name>` selection. Empty means workspace-default.
  packages ? [],
  # `cargo --exclude <name>` selection.
  excludePackages ? [],
  # Cargo subcommand intent the graph is being computed for. cargo's
  # resolver materialises slightly different unit sets for build vs
  # check vs test vs doc, so the cache key incorporates this and the
  # consumer must use the same value at lookup time.
  intent ? "build",
  ...
}:

let
  inherit (pkgs) lib;
  effectiveRustc = if rustToolchain != null then rustToolchain else pkgs.rustc;
  schneeBin = self.packages.${pkgs.stdenv.hostPlatform.system}.default;

  profileFlag =
    if buildType == "release" then [ "--release" ]
    else if buildType == "dev" then [ ]
    else [ "--profile" buildType ];

  packageFlags = lib.concatMap (p: [ "-p" p ]) packages;
  excludeFlags = lib.concatMap (p: [ "--exclude" p ]) excludePackages;
  featureFlags = lib.concatMap (f: [ "--features" f ]) features;
  targetFlag   = lib.optionals (target != null) [ "--target" target ];
  noDefaultFlag = lib.optionals noDefaultFeatures [ "--no-default-features" ];

  args = profileFlag
    ++ targetFlag
    ++ packageFlags
    ++ excludeFlags
    ++ featureFlags
    ++ noDefaultFlag
    ++ [ "--intent" intent ];

  argsStr = lib.escapeShellArgs args;

in
pkgs.runCommand "${baseNameOf (toString src)}-unit-graph" {
  inherit cargoDeps;
  nativeBuildInputs = [ effectiveRustc schneeBin ];
  # The graph derivation is small and CPU-bound — keep it in-process.
  preferLocalBuild = true;
  allowSubstitutes = true;
} ''
  cp -r ${src} workspace
  chmod -R u+w workspace
  cd workspace
  export cargoDepsCopy="$cargoDeps"
  mkdir -p "$out"
  ${schneeBin}/bin/cargo-schnee schnee compute-graph \
    --manifest-path Cargo.toml \
    --vendor-dir "$cargoDeps" \
    --output "$out/graph.json" \
    ${argsStr}
''
