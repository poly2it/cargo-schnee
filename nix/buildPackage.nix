# lib.buildPackage — first-class build API for cargo-schnee.
#
# Wraps buildRustPackage with cargo-schnee's toolchain wrapper, invariants,
# and a custom install phase.  Consumers pass a flat attribute set instead of
# wiring makeCargoWrapper + cargoOverrides + requiredSystemFeatures manually.
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
  extraSources ? {},
  env ? {},
  passthruEnv ? [],
  wrapBinaries ? false,
  doCheck ? true,
  preCheck ? "",
  postCheck ? "",
  buildType ? "release",
  preBuild ? "",
  postBuild ? "",
  postInstall ? "",
  postFixup ? "",
  meta ? {},
  ...
}@args:

let
  inherit (pkgs) lib;

  # -- cross-compilation --------------------------------------------------
  effectiveHostPkgs = if hostPkgs != null then hostPkgs else pkgs;
  isWindows = target != null && (lib.hasInfix "windows" target || lib.hasInfix "msvc" target);

  # CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER etc.
  cargoTargetEnvPrefix = lib.toUpper (builtins.replaceStrings ["-"] ["_"] target);

  # -- pname / version auto-detection ------------------------------------
  rootCargoToml = builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"));

  # For workspace builds with `package` set, find the member's Cargo.toml
  # by scanning workspace.members for a path whose Cargo.toml has a matching name.
  # Members may contain globs (e.g. "crates/*", "*"), so we expand those first.

  # Expand a single member pattern into concrete directory paths.
  # "crates/foo" (no glob)   -> [ "crates/foo" ]
  # "crates/*"               -> list subdirectories of <src>/crates/
  # "*"                      -> list subdirectories of <src>/
  # "crates/*/sub"           -> list <src>/crates/X/sub for each X
  #
  # Only a single '*' segment is supported (matching Cargo's glob behaviour).
  expandMember = m:
    let
      hasGlob = lib.hasInfix "*" m;
      # Split the pattern into segments around the glob.
      parts = lib.splitString "/*" m;
      # prefix: everything before the glob ("crates" for "crates/*", "" for "*")
      prefix = builtins.head parts;
      # suffix: everything after the glob ("/sub" for "crates/*/sub", "" for "crates/*")
      suffix = lib.concatStrings (builtins.tail parts);
      # Trim leading "/" from suffix if present
      cleanSuffix = lib.removePrefix "/" suffix;
      parentDir = if prefix == "*" || prefix == "" then src else src + "/${prefix}";
      entries = builtins.readDir parentDir;
      dirs = lib.filterAttrs (_: type: type == "directory") entries;
      expanded = builtins.filter (p: builtins.pathExists (src + "/${p}/Cargo.toml")) (
        map (name:
          let
            base = if prefix == "*" || prefix == "" then name else "${prefix}/${name}";
          in
            if cleanSuffix != "" then "${base}/${cleanSuffix}" else base
        ) (builtins.attrNames dirs)
      );
    in
      if hasGlob then expanded else [ m ];

  memberCargoToml =
    if package != null && (rootCargoToml ? workspace) then
      let
        memberPatterns = rootCargoToml.workspace.members or [];
        allMembers = builtins.concatMap expandMember memberPatterns;
        findMember = builtins.foldl' (acc: m:
          if acc != null then acc
          else
            let
              cargoPath = src + "/${m}/Cargo.toml";
            in
              if !builtins.pathExists cargoPath then null
              else
                let toml = builtins.fromTOML (builtins.readFile cargoPath);
                in if (toml.package.name or "") == package then toml else null
        ) null allMembers;
      in findMember
    else null;

  effectiveCargoToml =
    if memberCargoToml != null then memberCargoToml
    else rootCargoToml;

  detectedPname =
    if package != null then package
    else effectiveCargoToml.package.name or "unknown";
  rawVersion = effectiveCargoToml.package.version or null;
  detectedVersion = if builtins.isString rawVersion then rawVersion else "0.1.0";
  finalPname = if pname != null then pname else detectedPname;
  finalVersion = if version != null then version else detectedVersion;

  # -- toolchain ----------------------------------------------------------
  effectiveRustc = if rustToolchain != null then rustToolchain else pkgs.rustc;
  effectiveCargo = if rustToolchain != null then rustToolchain else pkgs.cargo;

  schneeToolchain = self.lib.makeCargoWrapper {
    inherit pkgs;
    rustToolchain = effectiveRustc;
    cargo = lib.getExe' effectiveCargo "cargo";
    overrides = self.lib.cargoOverrides { inherit pkgs; };
  };

  schneeRustPlatform = effectiveHostPkgs.makeRustPlatform {
    cargo = schneeToolchain;
    rustc = schneeToolchain;
  };

  # -- vendoring ----------------------------------------------------------
  vendorArgs =
    if cargoDeps != null then { inherit cargoDeps; }
    else if cargoLock != null then { cargoLock = { lockFile = cargoLock; }; }
    else if cargoHash != null then { inherit cargoHash; }
    else throw "cargo-schnee buildPackage: one of cargoLock, cargoHash, or cargoDeps must be provided";

  # -- cargo flags --------------------------------------------------------
  # Note: --target is NOT added here.  When `target` is set we override
  # buildPhase entirely (see below) to avoid conflicting with the host
  # --target that cargoBuildHook bakes in via @rustcTargetSpec@.
  cargoBuildFlags = cargoExtraArgs
    ++ lib.optionals (package != null) [ "-p" package ];

  # -- custom build phase for explicit target ------------------------------
  # cargoBuildHook always injects `--target <hostPlatform>`.  When the
  # caller passes a different `target` (e.g. x86_64-pc-windows-msvc) we
  # must bypass the hook and call cargo directly.
  targetBuildFlags = lib.concatStringsSep " " (
    [ "--target" target ]
    ++ lib.optionals (buildType == "release") [ "--release" ]
    ++ lib.optionals (buildType != "dev" && buildType != "release") [ "--profile" buildType ]
    ++ lib.optionals (package != null) [ "-p" package ]
    ++ cargoExtraArgs
  );

  buildPhaseForTarget = ''
    runHook preBuild
    cargo build ${targetBuildFlags}
    runHook postBuild
  '';

  checkPhaseForTarget = ''
    runHook preCheck
    cargo test ${targetBuildFlags}
    runHook postCheck
  '';

  # -- extraSources (postUnpack) ------------------------------------------
  # For each { "../sibling" = ./source; } entry:
  #  1. Copy source into workspace root (strip leading ../)
  #  2. Patch Cargo.toml to rewrite the relative path
  #  3. Update workspace exclude list
  sanitiseName = relPath:
    let stripped = builtins.replaceStrings ["../"] [""] relPath;
    in if stripped == relPath
       then throw "cargo-schnee buildPackage: extraSources keys must start with '../' (got '${relPath}')"
       else stripped;

  extraSourcesScript = lib.concatStringsSep "\n" (
    lib.mapAttrsToList (relPath: source:
      let inTreeName = sanitiseName relPath; in
      ''
        # extraSources: ${relPath} -> ${inTreeName}
        mkdir -p "$(dirname "$sourceRoot/${inTreeName}")"
        cp -r ${source} "$sourceRoot/${inTreeName}"
        chmod -R u+w "$sourceRoot/${inTreeName}"

        # Rewrite path dependency references in all Cargo.toml files
        find "$sourceRoot" -name Cargo.toml -exec \
          sed -i "s|${lib.escapeShellArg relPath}|${inTreeName}|g" {} +

        # Add to workspace exclude list (if workspace section exists)
        if grep -q '^\[workspace\]' "$sourceRoot/Cargo.toml" 2>/dev/null; then
          if grep -q 'exclude' "$sourceRoot/Cargo.toml"; then
            sed -i 's|exclude = \[|exclude = ["${inTreeName}", |' "$sourceRoot/Cargo.toml"
          else
            sed -i '/^\[workspace\]/a exclude = ["${inTreeName}"]' "$sourceRoot/Cargo.toml"
          fi
        fi
      ''
    ) extraSources
  );

  # -- install phase ------------------------------------------------------
  # Cargo maps the "dev" profile to the "debug" output directory.
  profileDir =
    if buildType == "release" then "release"
    else if buildType == "dev" then "debug"
    else buildType;

  # Cross-compiled output lands in target/<triple>/<profile>/.
  releaseDir =
    if target != null
    then "target/${target}/${profileDir}"
    else "target/${profileDir}";

  installPhaseWindows = ''
    runHook preInstall

    releaseDir="${releaseDir}"
    mkdir -p $out/bin

    for f in "$releaseDir"/*.exe; do
      [ -f "$f" ] || continue
      install -m755 "$f" "$out/bin/"
    done

    # Install PDB debug symbol files if present
    for f in "$releaseDir"/*.pdb; do
      [ -f "$f" ] || continue
      install -m644 "$f" "$out/bin/"
    done

    # Install DLLs if any
    for f in "$releaseDir"/*.dll; do
      [ -f "$f" ] || continue
      install -m755 "$f" "$out/bin/"
    done

    rmdir --ignore-fail-on-non-empty $out/bin 2>/dev/null || true

    runHook postInstall
  '';

  installPhaseNative = ''
    runHook preInstall

    releaseDir="${releaseDir}"
    mkdir -p $out/bin $out/lib

    for f in "$releaseDir"/*; do
      [ -f "$f" ] || continue
      [ -x "$f" ] || continue
      name="$(basename "$f")"
      case "$name" in build-script-*|*.d) continue ;; esac
      echo "$name" | grep -qE -- '-[0-9a-f]{16}$' && continue
      case "$name" in *.so|*.so.*|*.a|*.dylib) continue ;; esac
      install -m755 "$f" "$out/bin/"
    done

    # Install shared libraries if any
    for f in "$releaseDir"/lib*.so "$releaseDir"/lib*.so.* "$releaseDir"/lib*.a "$releaseDir"/lib*.dylib; do
      [ -f "$f" ] && install -m644 "$f" "$out/lib/" || true
    done

    rmdir --ignore-fail-on-non-empty $out/lib $out/bin 2>/dev/null || true

    runHook postInstall
  '';

  installPhase = if isWindows then installPhaseWindows else installPhaseNative;

  # -- wrapBinaries (postFixup) -------------------------------------------
  wrapBinariesScript = lib.optionalString wrapBinaries ''
    if [ -d "$out/bin" ]; then
      for bin in $out/bin/*; do
        [ -f "$bin" ] || continue
        wrapProgram "$bin" \
          --prefix LD_LIBRARY_PATH : "${lib.makeLibraryPath buildInputs}"
      done
    fi
  '';

  # -- Windows test runner (Wine) -----------------------------------------
  # When targeting Windows with doCheck, automatically configure Wine so
  # that consumers don't need to wire up the runner, HOME, DLL overrides,
  # and display vars manually.
  wineEnvAttrs = lib.optionalAttrs (isWindows && doCheck) {
    "CARGO_TARGET_${cargoTargetEnvPrefix}_RUNNER" = "wine";
    WINEDLLOVERRIDES = "mscoree=d;mshtml=d";
    DISPLAY = "";
  };

  winePreCheck = lib.optionalString (isWindows && doCheck) ''
    export HOME="$TMPDIR/wine-home"
    mkdir -p "$HOME"
    unset WAYLAND_DISPLAY
  '';

  # -- passthruEnv --------------------------------------------------------
  passthruEnvAttrs = lib.optionalAttrs (passthruEnv != []) {
    CARGO_SCHNEE_PASSTHRU_ENVS = builtins.concatStringsSep " " passthruEnv;
  };

  # -- extra args passthrough ---------------------------------------------
  # Forward unrecognised attributes (e.g. postUnpack, patches, …) to
  # buildRustPackage, excluding the ones we consumed above.
  consumedKeys = [
    "pkgs" "src" "cargoLock" "cargoHash" "cargoDeps"
    "pname" "version" "package" "hostPkgs" "target" "rustToolchain"
    "nativeBuildInputs" "buildInputs" "cargoExtraArgs"
    "extraSources" "env" "passthruEnv" "wrapBinaries" "doCheck"
    "preCheck" "postCheck"
    "buildType" "preBuild" "postBuild" "postInstall" "postFixup" "meta"
  ];
  extraAttrs = removeAttrs args consumedKeys;

in
  assert lib.assertMsg (!(wrapBinaries && isWindows))
    "cargo-schnee buildPackage: wrapBinaries is not supported for Windows targets (dontFixup is required for PE binaries)";

  schneeRustPlatform.buildRustPackage (vendorArgs // extraAttrs // {
    pname = finalPname;
    version = finalVersion;
    inherit src;
    inherit buildType;
    inherit cargoBuildFlags;
    inherit installPhase;
    inherit preBuild postBuild postInstall meta;

    nativeBuildInputs = [ pkgs.nix ]
      ++ nativeBuildInputs
      ++ lib.optionals wrapBinaries [ pkgs.makeWrapper ]
      ++ lib.optionals (isWindows && doCheck) [ pkgs.wineWow64Packages.stable ];

    inherit buildInputs;

    # cargo-schnee invariants — these must not leak to the consumer
    requiredSystemFeatures = [ "recursive-nix" ];
    NIX_CONFIG = "extra-experimental-features = flakes pipe-operators ca-derivations";
    auditable = false;
    inherit doCheck postCheck;
    preCheck = winePreCheck + preCheck;

    # Skip nixpkgs' cargoSetupPostUnpackHook (cp -Lr + chmod -R of the vendor
    # dir).  cargo-schnee reads $cargoDeps directly via --vendor-dir.
    dontCargoSetupPostUnpack = true;

    # Set cargoDepsCopy so cargoSetupPostPatchHook's Cargo.lock validation
    # still works against the original (read-only) vendor path.
    postUnpack = ''
      export cargoDepsCopy="$cargoDeps"
    '' + (args.postUnpack or "") + extraSourcesScript;

    postFixup = wrapBinariesScript + postFixup;

    env = wineEnvAttrs // env // passthruEnvAttrs;
  } // lib.optionalAttrs (target != null) {
    # Bypass cargoBuildHook/cargoCheckHook which inject the host --target.
    buildPhase = buildPhaseForTarget;
    checkPhase = checkPhaseForTarget;
  } // lib.optionalAttrs isWindows {
    # patchelf/strip don't work on PE binaries
    dontFixup = true;
  })
