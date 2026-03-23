{ pkgs, cargo, overrides ? {}, rustToolchain ? null }:

let
  inherit (pkgs) lib;

  # Convert "--manifest-path" to "__manifest_path"
  flagToVar = flag:
    "__" + builtins.replaceStrings ["-"] ["_"]
      (lib.removePrefix "--" flag);

  # Generate case arms for one forwarded arg (--flag value and --flag=value)
  mkForwardCase = flag:
    let var = flagToVar flag; in
    ''
      ${flag}) ${var}="$2"; args+=(${flag} "$2"); shift 2 ;;
          ${flag}=*) ${var}="''${1#${flag}=}"; args+=("$1"); shift ;;
    '';

  # Generate case arm for a boolean flag (no value, e.g. --no-default-features)
  mkForwardBoolCase = flag:
    let var = flagToVar flag; in
    ''${flag}) ${var}=1; args+=("$1"); shift ;;'';

  # Generate one subcommand override branch
  mkOverride = subcmd: { command, forwardArgs ? [], boolArgs ? [], setup ? "", postRun ? "" }:
    let
      allFlags = forwardArgs ++ boolArgs;
      varInits = builtins.concatStringsSep "\n    "
        (map (f: ''${flagToVar f}=""'') allFlags);
      forwardCases = builtins.concatStringsSep "\n        "
        ((map mkForwardCase forwardArgs) ++ (map mkForwardBoolCase boolArgs));
    in ''
    ${subcmd})
        shift
        args=()
        ${varInits}
        while [ $# -gt 0 ]; do
          case "$1" in
            ${forwardCases}
            --) args+=("--" "$@"); shift $#; break ;;
            --release) args+=("--release"); shift ;;
            *) shift ;;
          esac
        done
        ${setup}
        ${command} "''${args[@]}"
        __ec=$?
        ${postRun}
        exit $__ec
        ;;
    '';

  overrideBranches = builtins.concatStringsSep "\n    "
    (lib.mapAttrsToList mkOverride overrides);

  wrapperScript = pkgs.writeShellScriptBin "cargo" ''
    case "''${1:-}" in
      ${overrideBranches}
      *)
        exec ${cargo} "$@"
        ;;
    esac
  '';

in
  if rustToolchain != null then
    # Splice wrapper into toolchain so it shadows the toolchain's cargo on PATH
    pkgs.symlinkJoin {
      name = "rust-cargo-wrapped";
      paths = [ wrapperScript rustToolchain ];
      passthru = { inherit (rustToolchain) targetPlatforms badTargetPlatforms; };
    }
  else
    wrapperScript
