# lib.clippyPackage — first-class clippy API for cargo-schnee.
#
# Runs `cargo clippy` for a single package (by default) inside a sandboxed
# derivation.  Skips the cargo build step; clippy runs in checkPhase.
#
# `cargo clippy` is intercepted by the cargoOverrides wrapper and routed to
# `cargo schnee clippy`, which mirrors the regular check pipeline but swaps
# rustc for clippy-driver on local (workspace) compile units.  Dependency
# units stay rustc-compiled so their per-unit derivations are byte-shared
# with the corresponding buildPackage / testPackage runs.
{ self }:

{
  package ? null,
  # Clippy scope.  Default `["--package" package]` when `package` is set,
  # else `[]`.  Pass `["--workspace"]` to lint the whole workspace.
  clippyScope ? null,
  # Extra args to `cargo clippy`, before `--`.  Defaults to `--no-deps`
  # so external crates are not re-linted; pass [] to opt out.
  clippyExtraArgs ? [ "--no-deps" ],
  # Lint args appended after `--`.  Default fails the build on warnings.
  lintArgs ? [ "--deny" "warnings" ],
  ...
}@args:

let
  pkgs = args.pkgs;
  inherit (pkgs) lib;

  defaultScope =
    if package != null then [ "--package" package ] else [];
  effectiveScope = if clippyScope != null then clippyScope else defaultScope;

  cargoArgs = lib.escapeShellArgs (effectiveScope ++ clippyExtraArgs);
  postDashes = lib.escapeShellArgs lintArgs;

  forwardArgs = removeAttrs args [
    "clippyScope" "clippyExtraArgs" "lintArgs"
    "preBuild" "preCheck"
  ];

  # With dontBuild = true the entire buildPhase is skipped — including its
  # preBuild hook.  Splice the caller's preBuild into preCheck so source-
  # tree mutations (codegen injection, etc.) still happen before cargo
  # clippy runs; the working directory is identical at both points.
  callerPreBuild = args.preBuild or "";
  callerPreCheck = args.preCheck or "";
  effectivePreCheck =
    if callerPreBuild == ""
    then callerPreCheck
    else callerPreBuild + "\n" + callerPreCheck;
in
self.lib.buildPackage (forwardArgs // {
  inherit package;
  doCheck = true;
  dontBuild = true;
  wrapBinaries = false;

  # Override cargoCheckHook's default `cargo test` invocation.  The wrapper
  # routes `cargo clippy` through `cargo schnee clippy` which injects
  # --vendor-dir from $cargoDeps automatically — no manual config.toml.
  checkPhase = ''
    runHook preCheck
    cargo clippy ${cargoArgs} -- ${postDashes}
    runHook postCheck
  '';
  preCheck = effectivePreCheck;

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    touch $out/ok
    runHook postInstall
  '';
})
