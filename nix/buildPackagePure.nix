# nix/buildPackagePure.nix — proof-of-concept lib helper that drives a
# cargo-schnee build through dynamic derivations rather than recursive-nix.
#
# Status: prototype. Demonstrates the architectural separation that
# eliminates recursive-nix slot inversion (see plan in cargo-schnee
# git history); does NOT yet match `lib.buildPackage`'s feature set
# (no buildRustPackage integration, no doCheck, no Windows handling,
# no extraSources, no postBuild hooks).
#
# Pipeline:
#
#   1. `planner` derivation runs `cargo-schnee --plan-only` inside a
#      sandbox.  It registers all unit drvs to the daemon (which still
#      requires `recursive-nix` as a transport for `add_text_to_store`)
#      but does NOT call `nix-store --realise`.  Its output is a copy
#      of the root drv FILE contents (the ATerm-encoded derivation),
#      named `<pkg>-<intent>.drv`.  This single sandboxed run holds
#      one build user briefly; concurrent invocations no longer block
#      each other on user-pool acquisition.
#
#   2. `builtins.outputOf planner.outPath "out"` resolves the planner's
#      output as a derivation reference.  Nix realises the planner,
#      reads its output as a drv file, then builds that drv via the
#      *outer* scheduler — flat unit DAG, one global `max-jobs`, no
#      slot inversion possible regardless of CI concurrency.
#
# Compatible with `intent` values matching cargo-schnee subcommands:
# `build`, `check`, `test`, `clippy`, `doc`.  Intent-specific output
# selection (multiple roots) needs follow-up work.
{ self }:

{
  pkgs,
  src,
  # Pre-vendored crate-source derivation.  Mutually exclusive with
  # `cargoLock`; one of the two is required.
  cargoDeps ? null,
  # Path to `Cargo.lock`.  When set, deps are vendored via
  # `pkgs.rustPlatform.importCargoLock`.  Matches `lib.buildPackage`'s
  # call shape so consumers can swap helpers without restructuring.
  cargoLock ? null,
  # Override the derivation pname / version.  When null, both are
  # derived from the workspace's root Cargo.toml (or from the matching
  # workspace member when `package` is set).
  pname ? null,
  version ? null,
  # `-p <name>` selector.  Workspace builds with no selection pick all
  # default members per cargo's resolver; the planner emits all roots
  # but this prototype only consumes the first.
  package ? null,
  # Cross-compilation target triple, or null for the host.
  target ? null,
  # `release`, `dev`, or any custom profile name.
  buildType ? "release",
  # Mirror cargo `--features`.
  features ? [],
  # Mirror cargo `--no-default-features`.
  noDefaultFeatures ? false,
  # `cargo-schnee schnee <intent>`.  One of `build`, `check`, `test`,
  # `clippy`, `doc`.
  intent ? "build",
  # Output drv name prefix.  `null` derives from the source basename.
  name ? null,
  # Extra args passed to the cargo-schnee subcommand verbatim.  Useful
  # for things like `--exclude` that aren't first-class arguments here.
  cargoExtraArgs ? [],
  # Rust toolchain providing rustc, cargo, rustdoc, clippy-driver.
  # cargo-schnee's planner shells out to `rustc -vV` to detect the host
  # target.  `null` falls back to nixpkgs' system rustc.
  rustToolchain ? null,
  ...
}:

let
  inherit (pkgs) lib;
  schneeBin = self.packages.${pkgs.stdenv.hostPlatform.system}.default;

  # -- vendoring ----------------------------------------------------------
  effectiveCargoDeps =
    if cargoDeps != null then cargoDeps
    else if cargoLock != null then
      pkgs.rustPlatform.importCargoLock { lockFile = cargoLock; }
    else throw "cargo-schnee buildPackagePure: cargoLock or cargoDeps required";

  # -- pname / version auto-detection ------------------------------------
  # Read the root Cargo.toml.  If it's a single-package manifest
  # (`[package]` at root), use it.  If it's a workspace, look up the
  # member that matches `package` (mirrors `lib.buildPackage`'s logic
  # but without glob expansion — simple member paths only).
  rootCargoToml = builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"));

  memberCargoToml =
    if package != null && (rootCargoToml ? workspace) then
      let
        members = rootCargoToml.workspace.members or [];
        # Pick the first non-glob member whose Cargo.toml has the
        # matching package name.  Glob members like `crates/*` aren't
        # auto-expanded here; the consumer can pass `pname` explicitly.
        findMember = builtins.foldl' (acc: m:
          if acc != null then acc
          else if lib.hasInfix "*" m then null
          else
            let
              cargoPath = src + "/${m}/Cargo.toml";
            in
              if !builtins.pathExists cargoPath then null
              else
                let toml = builtins.fromTOML (builtins.readFile cargoPath);
                in if (toml.package.name or "") == package then toml else null
        ) null members;
      in findMember
    else null;

  effectiveCargoToml =
    if memberCargoToml != null then memberCargoToml
    else if rootCargoToml ? package then rootCargoToml
    else null;

  detectedPname =
    if effectiveCargoToml != null
    then effectiveCargoToml.package.name or null
    else null;
  detectedVersion =
    if effectiveCargoToml != null
    then
      let v = effectiveCargoToml.package.version or null;
      in if builtins.isString v then v else null
    else null;

  finalPname =
    if pname != null then pname
    else if detectedPname != null then detectedPname
    else if package != null then package
    else if name != null then name
    else baseNameOf (toString src);
  finalVersion =
    if version != null then version
    else if detectedVersion != null then detectedVersion
    else "0.0.0";

  # `outputOf` requires the planner's output to be a valid drv path —
  # name must end in `.drv`.
  drvName = "${finalPname}-${finalVersion}-${intent}.drv";

  # rustToolchain provides rustc, cargo, rustdoc, clippy-driver — all
  # needed by cargo-schnee's planner to extract the unit graph and
  # construct per-unit derivations.
  effectiveRustToolchain =
    if rustToolchain != null then rustToolchain else pkgs.rustc;

  profileFlag =
    if buildType == "release" then [ "--release" ]
    else if buildType == "dev" then [ ]
    else [ "--profile" buildType ];

  packageFlags = lib.optionals (package != null) [ "-p" package ];
  targetFlags = lib.optionals (target != null) [ "--target" target ];
  featureFlags = lib.concatMap (f: [ "--features" f ]) features;
  noDefaultFlag = lib.optionals noDefaultFeatures [ "--no-default-features" ];

  schneeArgs =
    profileFlag
    ++ targetFlags
    ++ packageFlags
    ++ featureFlags
    ++ noDefaultFlag
    ++ cargoExtraArgs;
  schneeArgsStr = lib.escapeShellArgs schneeArgs;

  planner = derivation {
    name = drvName;
    system = pkgs.stdenv.hostPlatform.system;
    builder = "${pkgs.bash}/bin/bash";
    args = [ "-c" ''
      set -e
      # cargo-schnee resolves `which bash` and bakes the resolved path
      # into every unit drv's builder.  `pkgs.bash` is the interactive
      # variant in nixpkgs and is not a valid builder dep — its closure
      # is not what unit drvs reference.  bashNonInteractive is the
      # right one: it's what nix's normal stdenv-built drvs use.
      export PATH=${effectiveRustToolchain}/bin:${pkgs.stdenv.cc}/bin:${pkgs.coreutils}/bin:${pkgs.bashNonInteractive}/bin:${pkgs.nix}/bin:${pkgs.gnutar}/bin:${pkgs.gzip}/bin:${pkgs.findutils}/bin:${pkgs.gnused}/bin:${pkgs.gnugrep}/bin

      # cargo-schnee's planner reads the workspace from the cwd.  Copy
      # in the source so it can be patched (Cargo.lock validation, vendor
      # symlinks) without touching the nix store.
      mkdir -p workspace
      cp -r ${src}/. workspace/
      chmod -R u+w workspace
      cd workspace

      # cargo plugin convention: when invoked directly, the binary
      # expects argv[1] to be the plugin name ("schnee").  Global flags
      # like --plan-only attach to the SchneeArgs subgroup and must
      # therefore come AFTER the plugin name.
      ${schneeBin}/bin/cargo-schnee schnee \
        --plan-only "$TMPDIR/plan-out.txt" \
        ${intent} \
        --vendor-dir "$cargoDeps" \
        ${schneeArgsStr}

      # Pick the first root drv.  cargo-schnee can emit multiple roots
      # (e.g. multi-package builds, separate test compile drvs); a
      # production-grade buildPackage will need to either pick the
      # intent-appropriate root or aggregate them.  For the prototype we
      # take the first.
      ROOT=$(head -1 "$TMPDIR/plan-out.txt")
      cp "$ROOT" "$out"
    '' ];

    cargoDeps = effectiveCargoDeps;

    # The planner needs daemon access from inside the sandbox to
    # register the unit drvs (`add_text_to_store`).  This is the *only*
    # remaining recursive-nix dependency in the new pipeline — it does
    # NOT call `nix-store --realise`, so it doesn't hold a build user
    # while waiting on sub-builds.
    requiredSystemFeatures = [ "recursive-nix" ];
    NIX_CONFIG =
      "extra-experimental-features = flakes ca-derivations dynamic-derivations pipe-operators";

    # Content-addressed text output so `outputOf` can interpret $out as
    # a drv file.  `outputHashMode = "text"` is required.
    __contentAddressed = true;
    outputHashMode = "text";
    outputHashAlgo = "sha256";
  };

  # `outputOf` is the dyn-derivations primitive: nix realises `planner`
  # first, reads its output as a derivation, then builds that.  Result
  # is the build of the cargo-schnee-emitted root drv, scheduled by the
  # outer nix daemon under one global `max-jobs`.
  rawOutput = builtins.outputOf planner.outPath "out";

  # cargo-schnee's per-unit drv emits the linker output directly with a
  # cargo-internal hash suffix (e.g. `greeter-3079f61af44b7b4e`) plus
  # `.d` / `.rmeta` siblings.  Standard nixpkgs convention is
  # `$out/bin/<name>`, so wrap the raw output in a thin install step
  # that picks the executable, strips the hash suffix, and lays it out
  # under bin/ (and lib/ for cdylib / staticlib outputs).  Mirrors the
  # filter logic in `lib.buildPackage`'s installPhaseNative but reads
  # from the root drv's $out rather than from cargo's target/release/.
  installed = pkgs.runCommand "${finalPname}-${finalVersion}" {
    inherit rawOutput;
    passthru = { inherit rawOutput planner; };
  } ''
    mkdir -p $out/bin $out/lib
    for f in $rawOutput/*; do
      [ -f "$f" ] || continue
      name="$(basename "$f")"
      case "$name" in
        *.d|*.rmeta|build-script-*|*-build-script|diagnostics) continue ;;
      esac
      clean="$(echo "$name" | ${pkgs.gnused}/bin/sed -E 's/-[0-9a-f]{16}$//')"
      case "$name" in
        *.so|*.so.*|*.a|*.dylib)
          install -m644 "$f" "$out/lib/$clean"
          ;;
        *)
          if [ -x "$f" ]; then
            install -m755 "$f" "$out/bin/$clean"
          fi
          ;;
      esac
    done
    rmdir --ignore-fail-on-non-empty $out/bin $out/lib 2>/dev/null || true
  '';

in
  installed
