# nix/testPackagePure.nix — test runner using the dyn-derivation
# pipeline.  Companion to `lib.buildPackagePure`.
#
# Produces a derivation whose `$out` is a marker file (`$out/passed`)
# created only when every compiled test binary in the package exits
# zero.  The test binaries themselves come from cargo-schnee in
# `intent = "test"` mode through the same `outputOf` chain that
# `buildPackagePure` uses, so concurrent invocations are immune to the
# recursive-nix slot inversion that bit lib.testPackage.
#
# Status: prototype.  Single-package focus; multi-test selection
# (cargo's `--test <name>`) is not exposed yet.  Test-time
# `cargoTestExtraArgs` (e.g. `-- --test-threads=1`) are forwarded as
# arguments to every test binary.
{ self }:

{
  pkgs,
  # Forwarded args after consuming our own, so the caller's
  # `cargoExtraArgs`, `cargoLock`, `pname`, etc. all reach
  # buildPackagePure unchanged.
  cargoTestExtraArgs ? [],
  ...
}@args:

let
  inherit (pkgs) lib;

  # buildPackagePure handles the heavy lifting: planner derivation in
  # `intent = "test"` mode, dyn-derivation chain to the realised test
  # binaries, install step that lays them out under `$out/bin/`.
  forwarded = removeAttrs args [ "cargoTestExtraArgs" ];
  built = self.lib.buildPackagePure (forwarded // {
    intent = "test";
  });

  testArgsStr = lib.escapeShellArgs cargoTestExtraArgs;

in
  pkgs.runCommand "${built.name}-test" {
    inherit built;
    passthru = { inherit built; };
  } ''
    set -euo pipefail
    mkdir -p $out

    found=0
    for bin in "$built"/bin/*; do
      [ -x "$bin" ] || continue
      found=1
      echo "Running $(basename "$bin")..."
      "$bin" ${testArgsStr}
    done

    if [ "$found" = 0 ]; then
      echo "no test binaries found in $built/bin" >&2
      exit 1
    fi

    touch $out/passed
  ''
