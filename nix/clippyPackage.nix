# lib.clippyPackage — first-class clippy API for cargo-schnee.
#
# Runs `cargo clippy` for a single package (by default) inside a sandboxed
# derivation.  Skips the cargo build step; clippy runs in checkPhase.
#
# Currently `cargo clippy` falls through the wrapper to real cargo because
# `clippy` is not in `cargoOverrides`; clippy-driver runs as the rustc
# wrapper directly inside this derivation's sandbox.  Vendored deps are
# wired via a manually-written .cargo/config.toml since cargo-schnee
# disables nixpkgs's cargoSetupPostUnpackHook.  Once cargo-schnee grows
# native `schnee clippy` support (see src/main.rs SchneeCommand::Clippy)
# and the override is registered, the manual config.toml step becomes a
# harmless no-op and per-unit derivation caching kicks in for free.
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

  forwardArgs = removeAttrs args [ "clippyScope" "clippyExtraArgs" "lintArgs" ];
in
self.lib.buildPackage (forwardArgs // {
  inherit package;
  doCheck = true;
  dontBuild = true;
  wrapBinaries = false;

  # Override cargoCheckHook's default `cargo test` invocation.  Wire
  # vendored deps via .cargo/config.toml since the wrapper does not inject
  # --vendor-dir for clippy yet.
  checkPhase = ''
    runHook preCheck

    mkdir -p .cargo
    cat > .cargo/config.toml <<EOF
    [source.crates-io]
    replace-with = "vendored-sources"
    [source.vendored-sources]
    directory = "$cargoDeps"
    EOF

    cargo --offline clippy ${cargoArgs} -- ${postDashes}
    runHook postCheck
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    touch $out/ok
    runHook postInstall
  '';
})
