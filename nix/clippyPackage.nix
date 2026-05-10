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

  # cargo-schnee's clippy subcommand recognises a narrower flag set
  # than `cargo clippy`.  Consumers built against the OLD buildPackage
  # routinely pass `--workspace` and `--all-targets`; the former is
  # cargo-schnee's default behaviour (no `-p` lints all workspace
  # members), and the latter is a known feature gap (cargo-schnee
  # doesn't yet plan test / example / bench targets — extending it
  # is upstream work).  Strip both so callers continue to work; if
  # `--all-targets` coverage matters, lint with intent="test" via
  # lib.testPackage as a stopgap.
  unsupportedClippyArgs = [ "--workspace" "--all-targets" ];
  filteredScope =
    lib.filter (a: !lib.elem a unsupportedClippyArgs) effectiveScope;
  filteredExtraArgs =
    lib.filter (a: !lib.elem a unsupportedClippyArgs) clippyExtraArgs;
  preCargoArgs = filteredScope ++ filteredExtraArgs;

  forwarded = removeAttrs args [
    "clippyScope" "clippyExtraArgs" "lintArgs"
  ];

  # Deduplicate args.  Skeptiva-style consumers prepend
  # commonArgs.cargoExtraArgs onto clippyExtraArgs in their wrapper,
  # which together with our own cargoExtraArgs forwarding leaves
  # `--no-default-features` (and similar bool flags) appearing
  # twice.  cargo-schnee's clippy CLI rejects duplicates.  Strip
  # adjacent duplicates rather than full set-dedup so positional
  # argument ordering for `--features X` style pairs is preserved.
  dedup = xs:
    builtins.foldl' (acc: x: if lib.elem x acc then acc else acc ++ [ x ])
      [] xs;

  built = self.lib.buildPackage (forwarded // {
    inherit package;
    intent = "clippy";
    cargoExtraArgs = dedup ((args.cargoExtraArgs or []) ++ preCargoArgs);
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
