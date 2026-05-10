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

  # Native tools available to the planner sandbox.  cc-wrapper picks
  # up `${stdenv.cc}/bin` and that's what cargo's build scripts find as
  # `cc`; rustToolchain provides rustc/cargo/rustdoc/clippy-driver.
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

  # Resolve `.dev` (or other) outputs preferentially for inputs that
  # ship `.pc` files in a separate output (the standard nixpkgs
  # multi-output convention).  Falls back to the main output if there's
  # no `.dev`.  Mirrors what stdenv's pkg-config setup hook does.
  pickOutput = output: pkg: pkg.${output} or pkg;
  pkgConfigPath = lib.makeSearchPath "lib/pkgconfig"
    (map (pickOutput "dev") buildInputs);
  cIncludePath = lib.makeSearchPath "include"
    (map (pickOutput "dev") buildInputs);
  libraryPath = lib.makeLibraryPath buildInputs;

  plannerName = "${finalPname}-${finalVersion}-${intent}-planner";

  # Planner derivation's $out is a directory containing:
  #   - plan.txt: one root drv path per line (cargo-schnee --plan-only).
  #   - <hash>-<unit>.drv: a copy of every root drv file referenced by
  #     plan.txt, byte-identical to the originals registered in the
  #     store via add_text_to_store.
  #
  # The drv copies let us build per-root wrapper derivations whose
  # `outputOf "out"` resolves to each cargo-schnee root drv.  Since the
  # wrapper bytes match the originals, dedup wins: the realised builds
  # are the same per-unit drvs the planner registered, scheduled by
  # nix's outer scheduler under one global max-jobs.
  planner = derivation {
    name = plannerName;
    system = pkgs.stdenv.hostPlatform.system;
    builder = "${pkgs.bash}/bin/bash";
    args = [ "-c" ''
      set -euo pipefail
      export PATH=${binPath}
      ${lib.optionalString (pkgConfigPath != "")
        "export PKG_CONFIG_PATH=${pkgConfigPath}"}
      ${lib.optionalString (cIncludePath != "")
        "export C_INCLUDE_PATH=${cIncludePath}"}
      ${lib.optionalString (libraryPath != "")
        "export LIBRARY_PATH=${libraryPath}"}
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

      mkdir -p "$out"
      cp "$TMPDIR/plan-out.txt" "$out/plan.txt"
      while IFS= read -r drv; do
        [ -n "$drv" ] || continue
        cp "$drv" "$out/$(basename "$drv")"
      done < "$TMPDIR/plan-out.txt"
    '' ];

    cargoDeps = effectiveCargoDeps;

    requiredSystemFeatures = [ "recursive-nix" ];
    NIX_CONFIG = "extra-experimental-features = "
      + "flakes ca-derivations dynamic-derivations pipe-operators";

    __contentAddressed = true;
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
  };

  # IFD: realise the planner at eval time, read its plan.txt to learn
  # which root drvs to outputOf.  Each line is an absolute /nix/store
  # path of a registered drv.  cargo-schnee emits one root per
  # bin / lib / test / doc / clippy unit produced by `cargo` for the
  # selected -p / scope, in cargo's own order.
  planLines = lib.filter (l: l != "") (
    lib.splitString "\n" (
      lib.removeSuffix "\n"
        (builtins.readFile "${planner}/plan.txt")));

  # Per-root wrapper derivations: each wrapper's text-output is
  # byte-identical to the cargo-schnee-emitted root drv file, so
  # `builtins.outputOf wrapper.outPath "out"` resolves the wrapper
  # to a derivation reference for that root.  The outer scheduler
  # then realises the underlying drv.
  #
  # KNOWN LIMITATION: this pattern hits a content-addressed
  # realisation conflict whenever any root drv is transitively
  # depended on by another plan-listed root — the wrapper realises
  # the inner drv at the wrapper's `-rootN`-suffixed path, while
  # the transitive realisation hits the inner drv's natural store
  # path.  For single-package builds with one [lib] + several
  # [[bin]] targets the lib root is filtered out below, which
  # works because the bins transitively realise the lib anyway.
  # For multi-crate workspaces with internal cross-deps (every
  # `cargo build/check/clippy --workspace`), this isn't enough —
  # the proper fix is to extend cargo-schnee's `--plan-only` to
  # emit a single aggregator drv that depends on every root and
  # produces a combined output, which buildPackage would
  # outputOf in one step.  Until then, workspace-mode clippy /
  # check / test will fail at realisation registration time.
  libUnitName = lib.replaceStrings [ "-" ] [ "_" ] finalPname;
  isLibRoot = drvPath:
    lib.hasSuffix "-${libUnitName}.drv"
      (baseNameOf (builtins.unsafeDiscardStringContext drvPath));
  binPlanLines = lib.filter (p: !isLibRoot p) planLines;
  effectivePlanLines =
    if binPlanLines == [] then planLines else binPlanLines;

  mkRootBuild = idx: drvPath:
    let
      origName = baseNameOf
        (builtins.unsafeDiscardStringContext drvPath);
      wrapper = derivation {
        name = "${finalPname}-${finalVersion}-root${toString idx}.drv";
        system = pkgs.stdenv.hostPlatform.system;
        builder = "${pkgs.bash}/bin/bash";
        args = [ "-c" ''
          ${pkgs.coreutils}/bin/cp ${planner}/${origName} $out
        '' ];
        __contentAddressed = true;
        outputHashMode = "text";
        outputHashAlgo = "sha256";
      };
    in
      builtins.outputOf wrapper.outPath "out";

  rootBuilds = lib.imap0 mkRootBuild effectivePlanLines;

  # -- install step ------------------------------------------------------
  isWindows = target != null
    && (lib.hasInfix "windows" target || lib.hasInfix "msvc" target);

  # Per-root layout copy.  Doc emits a `doc/` subtree of HTML; other
  # intents emit linker output (a hash-suffixed binary plus .d /
  # .rmeta sidecars).  Each root's loop runs against `$ROOT` injected
  # at install time.
  installRoot =
    if intent == "doc" then ''
      if [ -d "$ROOT/doc" ]; then
        # --no-preserve=mode so files copied from a read-only nix
        # store input land writeable, allowing subsequent doc roots
        # to merge into the same tree without permission errors.
        cp -r --no-preserve=mode "$ROOT/doc/." "$out/share/doc/"
      fi
    '' else if isWindows then ''
      for f in "$ROOT"/*.exe "$ROOT"/*.dll "$ROOT"/*.pdb; do
        [ -f "$f" ] || continue
        install -m755 "$f" "$out/bin/"
      done
    '' else ''
      for f in "$ROOT"/*; do
        [ -f "$f" ] || continue
        name="$(basename "$f")"
        case "$name" in
          *.d|*.rmeta|build-script-*|*-build-script|diagnostics) continue ;;
        esac
        # Strip the 16-hex content-hash suffix cargo appends to per-unit
        # compile outputs.
        clean="$(echo "$name" | sed -E 's/-[0-9a-f]{16}$//')"
        case "$name" in
          *.so|*.so.*|*.a|*.dylib)
            install -m644 "$f" "$out/lib/$clean"
            ;;
          *)
            if [ -x "$f" ]; then
              # Cargo compiles bins whose [[bin]] name contains dashes
              # to underscore-named files (Rust identifiers can't have
              # dashes); the user-facing dashed name is normally
              # provided as a sibling copy in target/release/, but
              # per-unit outputs only contain the compiled file.
              # Translate to the dashed form so consumers' meta-
              # .mainProgram references and `nix run` work.  Bins with
              # genuinely underscored names will lose the underscore;
              # workaround is to set [[bin]] name explicitly.
              dashed="$(echo "$clean" | tr _ -)"
              install -m755 "$f" "$out/bin/$dashed"
            fi
            ;;
        esac
      done
    '';

  installInit =
    if intent == "doc" then ''mkdir -p "$out/share/doc"''
    else ''mkdir -p "$out/bin" "$out/lib"'';

  installFinish =
    if intent == "doc" then ""
    else ''rmdir --ignore-fail-on-non-empty $out/bin $out/lib 2>/dev/null || true'';

  wrapBinariesScript = lib.optionalString (wrapBinaries && !isWindows) ''
    if [ -d "$out/bin" ]; then
      for bin in $out/bin/*; do
        [ -f "$bin" ] || continue
        wrapProgram "$bin" \
          --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath buildInputs}"
      done
    fi
  '';

  rootBuildLines = lib.concatMapStrings
    (root: ''
      ROOT=${root}
      ${installRoot}
    '')
    rootBuilds;

  installed = pkgs.runCommand "${finalPname}-${finalVersion}" {
    inherit meta;
    rootBuildPaths = rootBuilds;
    nativeBuildInputs =
      lib.optionals wrapBinaries [ pkgs.makeWrapper ];
    passthru = { inherit planner rootBuilds; };
  } ''
    set -euo pipefail
    ${installInit}
    ${rootBuildLines}
    ${installFinish}
    ${postInstall}
    ${wrapBinariesScript}
  '';

in
  installed
