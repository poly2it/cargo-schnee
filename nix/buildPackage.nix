# lib.buildPackage — first-class build API for cargo-schnee.
#
# Pipeline (post-recursive-nix-removal):
#
#   1. `planner` derivation runs `cargo-schnee --plan-only` inside its
#      sandbox.  It still uses the daemon's `add_text_to_store` RPC to
#      register all unit drvs (and so still requires the
#      `recursive-nix` system feature *for the planner only*) but does
#      NOT call `nix-store --realise`.  The sandbox holds one build
#      user briefly, exits, and never blocks on sub-builds.  Concurrent
#      planners therefore don't deadlock on the build-user pool.
#
#   2. `builtins.outputOf planner.outPath "out"` resolves to a
#      derivation reference: nix realises the planner first, reads its
#      output as a drv, then schedules that drv on the *outer*
#      scheduler — flat unit DAG, one global `max-jobs`, no slot
#      inversion possible regardless of CI concurrency.
#
#   3. A thin `runCommand` install step lays out the cargo-schnee root
#      drv's output under `$out/bin` and `$out/lib` in nixpkgs
#      convention, applies `postInstall`, and (optionally) wraps
#      binaries with `makeWrapper` for `wrapBinaries`.
#
# Replaces the previous buildRustPackage-based implementation, which
# wedged CI under concurrent invocations because every planner
# sandbox held a build user while waiting on its inner units' own
# user-pool acquisition.
{ self }:

{
  pkgs,
  src,
  cargoLock ? null,
  cargoHash ? null,
  cargoDeps ? null,
  pname ? null,
  version ? null,
  package ? null,
  hostPkgs ? null,
  target ? null,
  rustToolchain ? null,
  nativeBuildInputs ? [],
  buildInputs ? [],
  cargoExtraArgs ? [],
  # Args appended after `--` to the cargo-schnee subcommand.  Used by
  # clippyPackage to pass lint flags through to clippy-driver.
  postDashArgs ? [],
  extraSources ? {},
  env ? {},
  passthruEnv ? [],
  sourceRootPrefix ? null,
  wrapBinaries ? false,
  doCheck ? false,
  preCheck ? "",
  postCheck ? "",
  buildType ? "release",
  features ? [],
  noDefaultFeatures ? false,
  preBuild ? "",
  postBuild ? "",
  postInstall ? "",
  postFixup ? "",
  meta ? {},
  dontBuild ? false,
  installPhase ? null,
  # Cargo subcommand intent.  Default is `build`; consumers like
  # `lib.testPackage` and `lib.clippyPackage` override to `test` /
  # `clippy`.  Internal-ish — most callers use the `lib.*` wrappers.
  intent ? "build",
  ...
}@args:

let
  inherit (pkgs) lib;
  schneeBin = self.packages.${pkgs.stdenv.hostPlatform.system}.default;

  # -- features not yet supported in the dyn-derivation pipeline -----
  # cargoHash needs the cargoLock vendor path. doCheck inline runs the
  # test phase in the same drv as the build; the new pipeline splits
  # build and test into separate derivations, so use lib.testPackage
  # instead. postBuild / postCheck / postFixup were buildRustPackage
  # hook points with no equivalent in the direct-derivation model.
  unsupported = lib.filterAttrs (n: v: v) {
    "cargoHash" = cargoHash != null;
    "doCheck = true (use lib.testPackage)" = doCheck;
    "preCheck (use lib.testPackage)" = preCheck != "";
    "postCheck (use lib.testPackage)" = postCheck != "";
    "postBuild" = postBuild != "";
    "postFixup" = postFixup != "";
    "dontBuild" = dontBuild;
    "installPhase override" = installPhase != null;
    "hostPkgs (cross-compile not yet validated)" = hostPkgs != null;
  };
  _ = if unsupported != {} then
    throw ''
      cargo-schnee buildPackage: ${
        lib.concatStringsSep ", " (lib.attrNames unsupported)
      } not yet supported by the dyn-derivation pipeline.
      See cargo-schnee's plan for migration status.''
    else null;

  # -- vendoring ----------------------------------------------------------
  effectiveCargoDeps =
    if cargoDeps != null then cargoDeps
    else if cargoLock != null then
      pkgs.rustPlatform.importCargoLock { lockFile = cargoLock; }
    else throw "cargo-schnee buildPackage: cargoLock or cargoDeps required";

  # -- pname / version auto-detection ------------------------------------
  rootCargoToml = builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"));

  expandMember = m:
    let
      hasGlob = lib.hasInfix "*" m;
      parts = lib.splitString "/*" m;
      prefix = builtins.head parts;
      suffix = lib.concatStrings (builtins.tail parts);
      cleanSuffix = lib.removePrefix "/" suffix;
      parentDir =
        if prefix == "*" || prefix == "" then src else src + "/${prefix}";
      entries = builtins.readDir parentDir;
      dirs = lib.filterAttrs (_: type: type == "directory") entries;
      expanded = builtins.filter
        (p: builtins.pathExists (src + "/${p}/Cargo.toml"))
        (map (n:
          let base =
            if prefix == "*" || prefix == "" then n else "${prefix}/${n}";
          in if cleanSuffix != "" then "${base}/${cleanSuffix}" else base
        ) (builtins.attrNames dirs));
    in if hasGlob then expanded else [ m ];

  memberCargoToml =
    if package != null && (rootCargoToml ? workspace) then
      let
        memberPatterns = rootCargoToml.workspace.members or [];
        allMembers = builtins.concatMap expandMember memberPatterns;
        findMember = builtins.foldl' (acc: m:
          if acc != null then acc
          else
            let cargoPath = src + "/${m}/Cargo.toml";
            in if !builtins.pathExists cargoPath then null
              else
                let toml = builtins.fromTOML (builtins.readFile cargoPath);
                in if (toml.package.name or "") == package then toml else null
        ) null allMembers;
      in findMember
    else null;

  effectiveCargoToml =
    if memberCargoToml != null then memberCargoToml
    else if rootCargoToml ? package then rootCargoToml
    else null;

  detectedPname =
    if effectiveCargoToml != null
    then effectiveCargoToml.package.name or null else null;
  detectedVersion =
    if effectiveCargoToml != null then
      let v = effectiveCargoToml.package.version or null;
      in if builtins.isString v then v else null
    else null;

  # Note: don't fall back to `baseNameOf (toString src)` — when src is a
  # nix store path that ends up unsafe in derivation names (would imply
  # a cyclic store-path reference).  Pick a stable string instead and
  # rely on the consumer to pass `pname` explicitly for workspace-doc
  # builds where no [package] table exists at the root.
  finalPname =
    if pname != null then pname
    else if detectedPname != null then detectedPname
    else if package != null then package
    else "unknown";
  finalVersion =
    if version != null then version
    else if detectedVersion != null then detectedVersion
    else "0.0.0";

  # -- toolchain ----------------------------------------------------------
  effectiveRustToolchain =
    if rustToolchain != null then rustToolchain else pkgs.rustc;

  # -- cargo flags --------------------------------------------------------
  profileFlag =
    if buildType == "release" then [ "--release" ]
    else if buildType == "dev" then [ ]
    else [ "--profile" buildType ];
  packageFlags = lib.optionals (package != null) [ "-p" package ];
  targetFlags = lib.optionals (target != null) [ "--target" target ];
  featureFlags = lib.concatMap (f: [ "--features" f ]) features;
  noDefaultFlag = lib.optionals noDefaultFeatures [ "--no-default-features" ];

  schneeArgs =
    profileFlag ++ targetFlags ++ packageFlags ++ featureFlags
    ++ noDefaultFlag ++ cargoExtraArgs;
  schneeArgsStr = lib.escapeShellArgs schneeArgs;
  postDashArgsStr =
    if postDashArgs == [] then ""
    else "-- " + lib.escapeShellArgs postDashArgs;

  # -- extraSources injection (matches old behaviour) --------------------
  sanitiseName = relPath:
    let stripped = builtins.replaceStrings ["../"] [""] relPath;
    in if stripped == relPath
      then throw "cargo-schnee buildPackage: extraSources keys must start with '../' (got '${relPath}')"
      else stripped;

  extraSourcesScript = lib.concatStringsSep "\n" (lib.mapAttrsToList
    (relPath: source:
      let inTreeName = sanitiseName relPath; in ''
        # extraSources: ${relPath} -> ${inTreeName}
        mkdir -p "$(dirname "workspace/${inTreeName}")"
        cp -r ${source} "workspace/${inTreeName}"
        chmod -R u+w "workspace/${inTreeName}"
        find workspace -name Cargo.toml -exec \
          sed -i "s|${lib.escapeShellArg relPath}|${inTreeName}|g" {} +
        if grep -q '^\[workspace\]' "workspace/Cargo.toml" 2>/dev/null; then
          if grep -q 'exclude' "workspace/Cargo.toml"; then
            sed -i 's|exclude = \[|exclude = ["${inTreeName}", |' \
              "workspace/Cargo.toml"
          else
            sed -i '/^\[workspace\]/a exclude = ["${inTreeName}"]' \
              "workspace/Cargo.toml"
          fi
        fi
      '') extraSources);

  # -- sourceRootPrefix path remap --------------------------------------
  rootRemap =
    lib.optionalAttrs (sourceRootPrefix != null) { "" = sourceRootPrefix; };
  extraSourceRemaps = lib.mapAttrs'
    (relPath: _:
      let n = sanitiseName relPath; in lib.nameValuePair n n)
    extraSources;
  effectivePathPrefixRemaps = rootRemap // extraSourceRemaps;
  pathPrefixRemapsJson =
    if effectivePathPrefixRemaps != {}
    then builtins.toJSON
      (lib.mapAttrsToList (f: t: [f t]) effectivePathPrefixRemaps)
    else null;

  # -- planner env --------------------------------------------------------
  plannerEnv = env
    // lib.optionalAttrs (passthruEnv != []) {
      CARGO_SCHNEE_PASSTHRU_ENVS = builtins.concatStringsSep " " passthruEnv;
    }
    // lib.optionalAttrs (pathPrefixRemapsJson != null) {
      CARGO_SCHNEE_PATH_PREFIX_REMAPS = pathPrefixRemapsJson;
    };

  envExportLines = lib.concatMapStrings
    (n: ''export ${n}=${lib.escapeShellArg (toString plannerEnv.${n})}
'')
    (lib.attrNames plannerEnv);

  binPath = lib.makeBinPath ([
    effectiveRustToolchain
    pkgs.stdenv.cc
    pkgs.coreutils
    pkgs.bashNonInteractive
    pkgs.nix
    pkgs.gnutar
    pkgs.gzip
    pkgs.findutils
    pkgs.gnused
    pkgs.gnugrep
  ] ++ nativeBuildInputs);

  pkgConfigPath = lib.makeSearchPath "lib/pkgconfig" buildInputs;

  drvName = "${finalPname}-${finalVersion}-${intent}.drv";

  planner = derivation {
    name = drvName;
    system = pkgs.stdenv.hostPlatform.system;
    builder = "${pkgs.bash}/bin/bash";
    args = [ "-c" ''
      set -euo pipefail
      export PATH=${binPath}
      ${lib.optionalString (pkgConfigPath != "")
        "export PKG_CONFIG_PATH=${pkgConfigPath}"}
      ${envExportLines}

      mkdir -p workspace
      cp -r ${src}/. workspace/
      chmod -R u+w workspace

      ${extraSourcesScript}

      cd workspace

      # cargo-schnee's cargoSetupPostPatchHook compatibility: when the
      # vendor dir is read-only (it is — store path), point cargoDepsCopy
      # at the original so Cargo.lock validation can still run.
      export cargoDepsCopy="$cargoDeps"

      ${preBuild}

      # cargo plugin convention: argv[1] is the plugin name ("schnee").
      # Global flags like --plan-only attach to the SchneeArgs subgroup
      # and must come AFTER the plugin name.
      ${schneeBin}/bin/cargo-schnee schnee \
        --plan-only "$TMPDIR/plan-out.txt" \
        ${intent} \
        --vendor-dir "$cargoDeps" \
        ${schneeArgsStr} ${postDashArgsStr}

      # Pick the first root drv.  cargo-schnee can emit multiple roots
      # (e.g. multi-binary builds, tests of multiple packages); a future
      # refinement can disambiguate by --bin or by intent-specific kind.
      ROOT=$(head -1 "$TMPDIR/plan-out.txt")
      cp "$ROOT" "$out"
    '' ];

    cargoDeps = effectiveCargoDeps;

    requiredSystemFeatures = [ "recursive-nix" ];
    NIX_CONFIG = "extra-experimental-features = "
      + "flakes ca-derivations dynamic-derivations pipe-operators";

    __contentAddressed = true;
    outputHashMode = "text";
    outputHashAlgo = "sha256";
  };

  rawOutput = builtins.outputOf planner.outPath "out";

  # -- install step ------------------------------------------------------
  isWindows = target != null
    && (lib.hasInfix "windows" target || lib.hasInfix "msvc" target);

  # Doc derivation outputs include a `doc/` subtree of generated HTML.
  # Other intents (build/check/test/clippy) emit linker output: a
  # hash-suffixed binary plus .d / .rmeta sidecars.  Pick the layout
  # matching the intent.
  installScript =
    if intent == "doc" then ''
      mkdir -p $out/share/doc
      if [ -d "$rawOutput/doc" ]; then
        cp -r "$rawOutput/doc/." "$out/share/doc/"
      fi
    '' else if isWindows then ''
      mkdir -p $out/bin
      for f in $rawOutput/*.exe $rawOutput/*.dll $rawOutput/*.pdb; do
        [ -f "$f" ] || continue
        install -m755 "$f" "$out/bin/"
      done
    '' else ''
      mkdir -p $out/bin $out/lib
      for f in $rawOutput/*; do
        [ -f "$f" ] || continue
        name="$(basename "$f")"
        case "$name" in
          *.d|*.rmeta|build-script-*|*-build-script|diagnostics) continue ;;
        esac
        clean="$(echo "$name" | sed -E 's/-[0-9a-f]{16}$//')"
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

  wrapBinariesScript = lib.optionalString (wrapBinaries && !isWindows) ''
    if [ -d "$out/bin" ]; then
      for bin in $out/bin/*; do
        [ -f "$bin" ] || continue
        wrapProgram "$bin" \
          --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath buildInputs}"
      done
    fi
  '';

  installed = pkgs.runCommand "${finalPname}-${finalVersion}" {
    inherit rawOutput meta;
    nativeBuildInputs =
      lib.optionals wrapBinaries [ pkgs.makeWrapper ];
    passthru = { inherit rawOutput planner; };
  } ''
    set -euo pipefail
    ${installScript}
    ${postInstall}
    ${wrapBinariesScript}
  '';

in
  installed
