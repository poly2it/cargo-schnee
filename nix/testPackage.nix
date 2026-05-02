# lib.testPackage — first-class test API for cargo-schnee.
#
# Runs `cargo test` for a single package (by default) inside a sandboxed
# derivation, sharing per-unit compilation derivations bit-for-bit with the
# corresponding `buildPackage` invocation thanks to content addressing.
#
# Skips the `cargo build` step entirely; cargoCheckHook drives the test
# compilation and execution in one pass.
{ self }:

{
  package ? null,
  # Test scope.  Default is `["--package" package]` when `package` is set,
  # otherwise `[]` which lets cargo apply its own scoping (current package
  # by manifest, or the workspace).  Pass `["--workspace"]` to opt in to
  # the full graph, or `[]` explicitly to disable the implicit `--package`.
  testScope ? null,
  # Extra args appended after `<scope>`.  Use for `--lib`, `--features X`,
  # `-- --test-threads=1`, etc.
  cargoTestExtraArgs ? [],
  ...
}@args:

let
  pkgs = args.pkgs;
  inherit (pkgs) lib;

  defaultScope =
    if package != null then [ "--package" package ] else [];
  effectiveScope = if testScope != null then testScope else defaultScope;
  cargoTestFlags = effectiveScope ++ cargoTestExtraArgs;

  forwardArgs = removeAttrs args [ "testScope" "cargoTestExtraArgs" ];
in
self.lib.buildPackage (forwardArgs // {
  inherit package;
  doCheck = true;
  dontBuild = true;
  wrapBinaries = false;
  inherit cargoTestFlags;

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    touch $out/ok
    runHook postInstall
  '';
})
