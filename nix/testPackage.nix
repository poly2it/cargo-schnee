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
  # Store path of a shell script sourced in the runner before the first
  # test binary executes, under the unit setup script contract: its
  # exports persist into every test binary, an EXIT trap it sets fires
  # after the last binary exits (a defined shutdown point for
  # daemonised helpers), and SCHNEE_AUX_DIR names $out/schnee-aux for
  # auxiliary output.  A nonzero exit fails the run with that code.
  testRunnerSetup ? null,
  ...
}@args:

let
  inherit (pkgs) lib;

  defaultScope = if package != null then [ "--package" package ] else [];
  effectiveScope = if testScope != null then testScope else defaultScope;
  cargoArgs = effectiveScope ++ cargoTestExtraArgs;

  # Dedup forwarded args; some consumers' test wrappers prepend
  # commonArgs.cargoExtraArgs onto cargoTestExtraArgs in their wrapper,
  # so without dedup `--no-default-features` (and friends) end up
  # listed twice and cargo-schnee's clap rejects duplicates.  Order-
  # preserving so positional pairs like `--features X` are intact.
  dedup = xs:
    builtins.foldl' (acc: x: if lib.elem x acc then acc else acc ++ [ x ])
      [] xs;

  forwarded = removeAttrs args [
    "testScope" "cargoTestExtraArgs" "testRunnerArgs" "testRunnerSetup"
  ];

  built = self.lib.buildPackage (forwarded // {
    inherit package;
    intent = "test";
    cargoExtraArgs = dedup ((args.cargoExtraArgs or []) ++ cargoArgs);
  });

  runnerArgsStr = lib.escapeShellArgs testRunnerArgs;

  src = args.src;
  pkgArg = if package != null then "/${package}" else "";

in
  pkgs.runCommand "${built.name}-result" {
    inherit built src;
    passthru = { inherit built; };
  } ''
    set -euo pipefail
    mkdir -p $out

    ${lib.optionalString (testRunnerSetup != null) ''
      # Runner setup hook: sourced (not executed) so exports persist
      # into the test binaries and an EXIT trap fires after the last
      # binary exits.  $out exists; SCHNEE_AUX_DIR is the runner's
      # auxiliary output channel.
      export SCHNEE_AUX_DIR=$out/schnee-aux
      . ${testRunnerSetup}
    ''}
    # cargo-schnee compiles test binaries with `CARGO_MANIFEST_DIR`
    # set to `/proc/self/fd/1000`, which resolves to whatever the
    # running process holds open on descriptor 1000.  Open the crate
    # source there, so tests using
    # `env!("CARGO_MANIFEST_DIR").join("testdata/...")` find their
    # fixtures.  Best-effort: `$src/<package>` if that exists, else
    # `$src`.  Tests requiring workspace-root-prefixed paths need the
    # consumer to lay out src to match, for example by using the
    # workspace directory as src.
    target="$src"
    if [ -n "${pkgArg}" ] && [ -d "$src${pkgArg}" ]; then
      target="$src${pkgArg}"
    fi
    exec 1000<"$target"
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
