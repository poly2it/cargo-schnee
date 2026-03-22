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

  # Generate one subcommand override branch
  mkOverride = subcmd: { command, forwardArgs ? [], setup ? "", postRun ? "" }:
    let
      varInits = builtins.concatStringsSep "\n    "
        (map (f: ''${flagToVar f}=""'') forwardArgs);
      forwardCases = builtins.concatStringsSep "\n        "
        (map mkForwardCase forwardArgs);
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
