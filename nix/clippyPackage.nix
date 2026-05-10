# lib.clippyPackage — first-class clippy API for cargo-schnee.
#
# Routes through `lib.buildPackage` with `intent = "clippy"`, which
# swaps rustc for clippy-driver on workspace compile units.  Lint
# args after `--` are forwarded to clippy-driver via buildPackage's
# `postDashArgs`.  Dependency units stay rustc-compiled so their
# per-unit derivations are byte-shared with regular check / build
# runs.
#
# Build success means lint clean (when `--deny warnings` is in
# `lintArgs`, the default).  `$out/ok` marks success.
{ self }:

{
  pkgs,
  package ? null,
  # Clippy scope.  Default `["--package" package]` when `package` is
  # set, else `[]`.  Matches the previous clippyPackage API.
  clippyScope ? null,
  # Extra args to `cargo schnee clippy`, before `--`.  Defaults to
  # `--no-deps` so external crates are not re-linted.
  clippyExtraArgs ? [ "--no-deps" ],
  # Lint args appended after `--`.  Default fails the build on
  # warnings.
  lintArgs ? [ "--deny" "warnings" ],
  ...
}@args:

let
  inherit (pkgs) lib;

  defaultScope = if package != null then [ "--package" package ] else [];
  effectiveScope = if clippyScope != null then clippyScope else defaultScope;
  preCargoArgs = effectiveScope ++ clippyExtraArgs;

  forwarded = removeAttrs args [
    "clippyScope" "clippyExtraArgs" "lintArgs"
  ];

  built = self.lib.buildPackage (forwarded // {
    inherit package;
    intent = "clippy";
    cargoExtraArgs = (args.cargoExtraArgs or []) ++ preCargoArgs;
    postDashArgs = (args.postDashArgs or []) ++ lintArgs;
  });

in
  pkgs.runCommand "${built.name}-clippy-ok" {
    inherit built;
    passthru = { inherit built; };
  } ''
    mkdir -p $out
    # Clippy fails the build itself when --deny warnings triggers, so
    # reaching this point means lint clean.
    [ -d "$built" ] && touch $out/ok
  ''
