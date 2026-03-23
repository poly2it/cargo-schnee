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
  rustToolchain ? null,
  nativeBuildInputs ? [],
  buildInputs ? [],
  cargoExtraArgs ? [],
  extraSources ? {},
  env ? {},
  passthruEnv ? [],
  wrapBinaries ? false,
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

  # -- pname / version auto-detection ------------------------------------
  rootCargoToml = builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"));

  # For workspace builds with `package` set, find the member's Cargo.toml
  # by scanning workspace.members for a path whose Cargo.toml has a matching name.
  memberCargoToml =
    if package != null && (rootCargoToml ? workspace) then
      let
        members = rootCargoToml.workspace.members or [];
        findMember = builtins.foldl' (acc: m:
          if acc != null then acc
          else
            let
              toml = builtins.fromTOML (builtins.readFile (src + "/${m}/Cargo.toml"));
            in
              if (toml.package.name or "") == package then toml else null
        ) null members;
      in findMember
    else null;

  effectiveCargoToml =
    if memberCargoToml != null then memberCargoToml
    else rootCargoToml;

  detectedPname =
    if package != null then package
    else effectiveCargoToml.package.name or "unknown";
  detectedVersion = effectiveCargoToml.package.version or "0.1.0";
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

  schneeRustPlatform = pkgs.makeRustPlatform {
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
  cargoBuildFlags = cargoExtraArgs
    ++ lib.optionals (package != null) [ "-p" package ];

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
  profileDir = if buildType == "release" then "release" else buildType;

  installPhase = ''
    runHook preInstall

    releaseDir="target/${profileDir}"
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

  # -- passthruEnv --------------------------------------------------------
  passthruEnvAttrs = lib.optionalAttrs (passthruEnv != []) {
    CARGO_SCHNEE_PASSTHRU_ENVS = builtins.concatStringsSep " " passthruEnv;
  };

  # -- extra args passthrough ---------------------------------------------
  # Forward unrecognised attributes (e.g. postUnpack, patches, …) to
  # buildRustPackage, excluding the ones we consumed above.
  consumedKeys = [
    "pkgs" "src" "cargoLock" "cargoHash" "cargoDeps"
    "pname" "version" "package" "rustToolchain"
    "nativeBuildInputs" "buildInputs" "cargoExtraArgs"
    "extraSources" "env" "passthruEnv" "wrapBinaries"
    "buildType" "preBuild" "postBuild" "postInstall" "postFixup" "meta"
  ];
  extraAttrs = removeAttrs args consumedKeys;

in
  schneeRustPlatform.buildRustPackage (vendorArgs // extraAttrs // {
    pname = finalPname;
    version = finalVersion;
    inherit src;
    inherit buildType;
    inherit cargoBuildFlags;
    inherit installPhase;
    inherit preBuild postBuild meta;

    nativeBuildInputs = [ pkgs.nix ]
      ++ nativeBuildInputs
      ++ lib.optionals wrapBinaries [ pkgs.makeWrapper ];

    inherit buildInputs;

    # cargo-schnee invariants — these must not leak to the consumer
    requiredSystemFeatures = [ "recursive-nix" ];
    NIX_CONFIG = "extra-experimental-features = flakes pipe-operators ca-derivations";
    auditable = false;
    doCheck = false;

    postUnpack = (args.postUnpack or "") + extraSourcesScript;

    postFixup = wrapBinariesScript + postFixup;

    env = env // passthruEnvAttrs;
  })
