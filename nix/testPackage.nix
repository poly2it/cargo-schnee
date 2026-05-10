# lib.testPackage — first-class test API for cargo-schnee.
#
# Builds the test binaries for a single package (by default) via the
# same dyn-derivation pipeline `lib.buildPackage` uses, then executes
# them in a downstream `runCommand` whose `$out` is a pass marker.
# Concurrent test invocations share recursive-nix-free realisation
# semantics with their build counterparts — no slot inversion.
{ self }:

{
  pkgs,
  package ? null,
  # Test scope.  Default is `["--package" package]` when `package` is
  # set, else `[]`.  Matches the previous testPackage API.
  testScope ? null,
  # Extra args appended to the cargo-schnee subcommand.  Use for
  # `--lib`, `--features X`, etc.
  cargoTestExtraArgs ? [],
  # Args passed to each test binary at run time.  Use for
  # `--test-threads=1`, `--nocapture`, filtering, etc.
  testRunnerArgs ? [],
  ...
}@args:

let
  inherit (pkgs) lib;

  defaultScope = if package != null then [ "--package" package ] else [];
  effectiveScope = if testScope != null then testScope else defaultScope;
  cargoArgs = effectiveScope ++ cargoTestExtraArgs;

  forwarded = removeAttrs args [
    "testScope" "cargoTestExtraArgs" "testRunnerArgs"
  ];

  built = self.lib.buildPackage (forwarded // {
    inherit package;
    intent = "test";
    cargoExtraArgs = (args.cargoExtraArgs or []) ++ cargoArgs;
  });

  runnerArgsStr = lib.escapeShellArgs testRunnerArgs;

in
  pkgs.runCommand "${built.name}-result" {
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
      "$bin" ${runnerArgsStr}
    done

    if [ "$found" = 0 ]; then
      echo "no test binaries found in $built/bin" >&2
      exit 1
    fi

    touch $out/ok
  ''
