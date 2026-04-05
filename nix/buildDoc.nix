# lib.buildDoc — build rustdoc documentation via cargo-schnee.
#
# Shares the same source setup as buildPackage, including vendoring,
# extraSources, and toolchain wrapping, but runs `cargo doc` instead of
# `cargo build` and installs the generated HTML into $out/share/doc.
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
  # Doc-specific options
  documentPrivateItems ? false,
  noDeps ? true,
  preBuild ? "",
  postBuild ? "",
  postInstall ? "",
  meta ? {},
  ...
}@args:

let
  inherit (pkgs) lib;

  # -- pname / version auto-detection ------------------------------------
  rootCargoToml = builtins.fromTOML (builtins.readFile (src + "/Cargo.toml"));

  # For workspace builds with `package` set, find the member's Cargo.toml.
  expandMember = m:
    let
      hasGlob = lib.hasInfix "*" m;
      parts = lib.splitString "/*" m;
      prefix = builtins.head parts;
      suffix = lib.concatStrings (builtins.tail parts);
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

  schneeRustPlatform = pkgs.makeRustPlatform {
    cargo = schneeToolchain;
    rustc = schneeToolchain;
  };

  # -- vendoring ----------------------------------------------------------
  vendorArgs =
    if cargoDeps != null then { inherit cargoDeps; }
    else if cargoLock != null then { cargoLock = { lockFile = cargoLock; }; }
    else if cargoHash != null then { inherit cargoHash; }
    else throw "cargo-schnee buildDoc: one of cargoLock, cargoHash, or cargoDeps must be provided";

  # -- cargo flags --------------------------------------------------------
  cargoBuildFlags = cargoExtraArgs
    ++ lib.optionals (package != null) [ "-p" package ];

  # -- extraSources (postUnpack) ------------------------------------------
  sanitiseName = relPath:
    let stripped = builtins.replaceStrings ["../"] [""] relPath;
    in if stripped == relPath
       then throw "cargo-schnee buildDoc: extraSources keys must start with '../' (got '${relPath}')"
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

  # -- doc flags ----------------------------------------------------------
  docFlags = lib.concatStringsSep " " (
    lib.optionals noDeps [ "--no-deps" ]
    ++ lib.optionals documentPrivateItems [ "--document-private-items" ]
    ++ lib.optionals (package != null) [ "-p" package ]
    ++ cargoExtraArgs
  );

  # -- passthruEnv --------------------------------------------------------
  passthruEnvAttrs = lib.optionalAttrs (passthruEnv != []) {
    CARGO_SCHNEE_PASSTHRU_ENVS = builtins.concatStringsSep " " passthruEnv;
  };

  # -- extra args passthrough ---------------------------------------------
  consumedKeys = [
    "pkgs" "src" "cargoLock" "cargoHash" "cargoDeps"
    "pname" "version" "package" "rustToolchain"
    "nativeBuildInputs" "buildInputs" "cargoExtraArgs"
    "extraSources" "env" "passthruEnv"
    "documentPrivateItems" "noDeps"
    "preBuild" "postBuild" "postInstall" "meta"
  ];
  extraAttrs = removeAttrs args consumedKeys;

in
  schneeRustPlatform.buildRustPackage (vendorArgs // extraAttrs // {
    pname = "${finalPname}-doc";
    version = finalVersion;
    inherit src;
    inherit cargoBuildFlags;
    inherit preBuild postBuild postInstall meta;

    nativeBuildInputs = [ pkgs.nix ] ++ nativeBuildInputs;
    inherit buildInputs;

    # cargo-schnee invariants
    requiredSystemFeatures = [ "recursive-nix" ];
    NIX_CONFIG = "extra-experimental-features = flakes pipe-operators ca-derivations";
    auditable = false;
    doCheck = false;

    # Skip nixpkgs' cargoSetupPostUnpackHook
    dontCargoSetupPostUnpack = true;

    postUnpack = ''
      export cargoDepsCopy="$cargoDeps"
    '' + (args.postUnpack or "") + extraSourcesScript;

    buildPhase = ''
      runHook preBuild
      cargo doc ${docFlags}
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out/share/doc
      cp -r target/doc/. $out/share/doc/
      runHook postInstall
    '';

    env = env // passthruEnvAttrs;
  })
