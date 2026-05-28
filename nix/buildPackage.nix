# lib.buildPackage — first-class build API for cargo-schnee.
#
# Pipeline:
#
#   1. The `planner` derivation runs `cargo-schnee --plan-only` inside
#      its sandbox.  Registration uses the daemon's `add_text_to_store`
#      RPC over the bind-mounted socket, so the planner requires the
#      `recursive-nix` system feature.  It registers every unit drv
#      plus an aggregator drv that depends on the workspace roots, then
#      exits; it never calls `nix-store --realise`.  Registration is
#      pure metadata, so the sandbox holds one build user briefly and
#      never blocks on sub-builds.  Concurrent planners do not deadlock
#      on the build-user pool.
#
#   2. `builtins.outputOf aggregatorWrapper.outPath "out"` resolves to
#      the aggregator drv.  Nix realises the planner first, reads the
#      aggregator drv file the planner copied into its `$out`, and
#      schedules that drv on the outer scheduler.  The unit DAG is flat
#      under one global `max-jobs`, with no slot inversion regardless
#      of CI concurrency.
#
#   3. A thin `runCommand` install step lays out the cargo-schnee root
#      drv's output under `$out/bin` and `$out/lib` in nixpkgs
#      convention, applies `postInstall`, and wraps binaries with
#      `makeWrapper` when `wrapBinaries` is set.
#
# This replaces the previous buildRustPackage-based implementation,
# which wedged CI under concurrent invocations: every planner sandbox
# held a build user while waiting on its inner units' own user-pool
# acquisition, a classic resource-ordering deadlock.  The new pipeline
# confines recursive-nix to the brief, leaf-only registration step,
# which never recurses into builds.
#
# `recursive-nix` is therefore part of the planner's contract, not a
# residual cost to be eliminated.  Truly removing it requires
# constructing one wrapper derivation per unit at nix eval time and
# chaining `builtins.outputOf` references through them; that is
# architecturally feasible but adds substantial nix-eval-time overhead
# without a correctness or concurrency benefit over the current shape.
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
      # crates.io's API endpoint (`crates.io/api/v1/crates`) 403s any
      # User-Agent containing `curl/...` (and `python-requests/...`) as
      # an anti-abuse measure since April 2026 — see rust-lang/crates.io#13482.
      # `pkgs.rustPlatform.importCargoLock` fetches through stock
      # `fetchurl`, whose builder sends `curl/<ver> Nixpkgs/<ver>`, so
      # every crate fetch fails with 403 until either nixpkgs ships the
      # matching UA fix (NixOS/nixpkgs#512735 covers fetchCargoVendor
      # only; fetchurl is still pending on the nixpkgs revision pinned
      # downstream of this) or the crate's output path is already in
      # the local store from a previous run.
      #
      # Override the registry download URL via `extraRegistries` (right-
      # biased merge over the default mapping) so requests go to
      # `static.crates.io`, which serves the same tarballs and does not
      # apply the UA filter. URL template is `<download>/<name>/<ver>/
      # download`, identical between the two hosts, so the recorded
      # `outputHash`es in downstream `Cargo.lock`s remain valid and
      # already-cached output paths are reused.
      pkgs.rustPlatform.importCargoLock {
        lockFile = cargoLock;
        extraRegistries = {
          "https://github.com/rust-lang/crates.io-index" =
            "https://static.crates.io/crates";
        };
      }
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

  # Workspace inheritance: a member crate can declare
  # `version.workspace = true` and pick up the version from the
  # root `[workspace.package].version`.  Resolve that here so the
  # derivation name doesn't fall back to "0.0.0" for inherited
  # versions.
  workspaceVersion =
    rootCargoToml.workspace.package.version or null;
  detectedVersion =
    if effectiveCargoToml != null then
      let v = effectiveCargoToml.package.version or null;
      in
        if builtins.isString v then v
        else if builtins.isAttrs v && (v.workspace or false)
                && builtins.isString workspaceVersion
        then workspaceVersion
        else null
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
        --plan-aggregator-out "$TMPDIR/aggregator.txt" \
        ${intent} \
        --vendor-dir "$cargoDeps" \
        ${schneeArgsStr} ${postDashArgsStr}

      mkdir -p "$out"
      cp "$TMPDIR/plan-out.txt" "$out/plan.txt"
      cp "$TMPDIR/aggregator.txt" "$out/aggregator.txt"
      # Copy the aggregator drv file under a FIXED filename so the
      # wrapper-cp pattern below does not have to learn the name
      # via `builtins.readFile`. Reading the file at eval time is an
      # IFD that forces realising this planner derivation before the
      # rest of the package can be evaluated; under Nix master's
      # stricter handling of floating-output derivations this fails
      # with "cannot operate on output 'out' of the unbuilt
      # derivation". The bytes the wrapper copies are unchanged, so
      # the content-addressed text-output hash (and therefore the
      # `outputOf` chain) is identical.
      AGG=$(${pkgs.coreutils}/bin/head -1 "$TMPDIR/aggregator.txt")
      cp "$AGG" "$out/aggregator.drv"
    '' ];

    cargoDeps = effectiveCargoDeps;

    requiredSystemFeatures = [ "recursive-nix" ];
    NIX_CONFIG = "extra-experimental-features = "
      + "flakes ca-derivations dynamic-derivations pipe-operators";

    __contentAddressed = true;
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
  };

  # Wrap the aggregator drv in a tiny text-output drv whose `$out` is
  # a byte-identical copy of the registered aggregator file.  Then
  # `builtins.outputOf wrapper.outPath "out"` resolves to the
  # aggregator's realisation, which the outer daemon builds — and
  # transitively builds every root.
  #
  # The planner emits the aggregator drv at a fixed `$out/aggregator.drv`
  # so we can reference it via a plain string interpolation. The
  # earlier design read `aggregator.txt` via `builtins.readFile`,
  # which is IFD: it forces the planner to be realised at eval time.
  # Nix master's stricter handling of floating-output derivations
  # rejects that with "cannot operate on output 'out' of the unbuilt
  # derivation", so every downstream `nix build .#<pkg>` failed.
  aggregatorWrapper = derivation {
    name = "${finalPname}-${finalVersion}-aggregator.drv";
    system = pkgs.stdenv.hostPlatform.system;
    builder = "${pkgs.bash}/bin/bash";
    args = [ "-c" ''
      ${pkgs.coreutils}/bin/cp ${planner}/aggregator.drv $out
    '' ];
    __contentAddressed = true;
    outputHashMode = "text";
    outputHashAlgo = "sha256";
  };

  aggregatorOutput = builtins.outputOf aggregatorWrapper.outPath "out";

  # -- install step ------------------------------------------------------
  isWindows = target != null
    && (lib.hasInfix "windows" target || lib.hasInfix "msvc" target);

  # Per-root layout copy.  Doc emits a `doc/` subtree of HTML; other
  # intents emit linker output (a hash-suffixed binary plus .d /
  # .rmeta sidecars).  Each root's loop runs against `$ROOT` and
  # `$TARGET_NAME` injected at install time.  `$TARGET_NAME` is the
  # canonical name cargo reports via `unit.target.name()`, propagated
  # through the aggregator's `root-N.target_name` metadata file.
  # Using it directly is the single source of truth for naming and
  # avoids the brittle `_→-` filename heuristic that corrupts bins
  # with genuinely underscored target names.
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
        ext="''${f##*.}"
        mode=755
        [ "$ext" = "pdb" ] && mode=644
        install -m"$mode" "$f" "$out/bin/''${TARGET_NAME}.''${ext}"
      done
    '' else ''
      for f in "$ROOT"/*; do
        [ -f "$f" ] || continue
        name="$(basename "$f")"
        case "$name" in
          *.d|*.rmeta|build-script-*|*-build-script|diagnostics) continue ;;
        esac
        case "$name" in
          *.so|*.so.*|*.a|*.dylib)
            # Native libs: keep cargo's `lib<crate>.<ext>` convention.
            # Strip the per-unit hash; the underscored crate name is
            # the canonical form for libs, so no further translation.
            clean="$(echo "$name" | sed -E 's/-[0-9a-f]{16}//')"
            install -m644 "$f" "$out/lib/$clean"
            ;;
          *)
            if [ -x "$f" ]; then
              install -m755 "$f" "$out/bin/''${TARGET_NAME}"
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

  installed = pkgs.runCommand "${finalPname}-${finalVersion}" {
    inherit meta;
    aggregator = aggregatorOutput;
    # Forward the caller's `nativeBuildInputs` so their setup hooks
    # (e.g. `makeWrapper`'s `wrapProgram`, `installShellFiles`) load in
    # `postInstall`.  Without this, callers can't run nixpkgs idioms
    # like `wrapProgram $out/bin/foo --prefix PATH : …` from
    # `postInstall` even after declaring `pkgs.makeWrapper` in
    # `nativeBuildInputs` — the build phase honours the declaration
    # but the install step does not.
    nativeBuildInputs = nativeBuildInputs
      ++ lib.optionals wrapBinaries [ pkgs.makeWrapper ];
    passthru = { inherit planner aggregatorWrapper aggregatorOutput; };
  } ''
    set -euo pipefail
    ${installInit}
    # Walk each root-N symlink in the aggregator output.  Each
    # symlink target is a single cargo-schnee root drv's $out
    # (binary, lib, doc subtree, etc.).  The sibling `root-N.target_name`
    # text file (a regular file, filtered out by the `-d` guard) carries
    # cargo's canonical target name for use in the rename below.
    for ROOT in $aggregator/root-*; do
      [ -d "$ROOT" ] || continue
      TARGET_NAME="$(cat "''${ROOT}.target_name" 2>/dev/null || true)"
      if [ -z "$TARGET_NAME" ] && [ "${intent}" != "doc" ]; then
        echo "cargo-schnee: aggregator emitted no target_name for $ROOT" >&2
        exit 1
      fi
      ${installRoot}
    done
    ${installFinish}
    ${postInstall}
    ${wrapBinariesScript}
  '';

in
  installed
