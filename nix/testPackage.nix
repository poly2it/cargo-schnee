# lib.testPackage — first-class test API for cargo-schnee.
#
# Builds the test binaries for a single package (by default) via the
# same dyn-derivation pipeline `lib.buildPackage` uses, then executes
# them in a downstream `runCommand` whose `$out` is a pass marker.
# Concurrent test invocations share recursive-nix-free realisation
# semantics with their build counterparts — no slot inversion.
{ self }:

{
  pkgs,
  package ? null,
  # Test scope.  Default is `["--package" package]` when `package` is
  # set, else `[]`.  Matches the previous testPackage API.
  testScope ? null,
  # Extra args appended to the cargo-schnee subcommand.  Use for
  # `--lib`, `--features X`, etc.
  cargoTestExtraArgs ? [],
  # Args passed to each test binary at run time.  Use for
  # `--test-threads=1`, `--nocapture`, filtering, etc.
  testRunnerArgs ? [],
  ...
}@args:

let
  inherit (pkgs) lib;

  defaultScope = if package != null then [ "--package" package ] else [];
  effectiveScope = if testScope != null then testScope else defaultScope;
  cargoArgs = effectiveScope ++ cargoTestExtraArgs;

  # Dedup forwarded args; consumers like skeptiva's testCrate prepend
  # commonArgs.cargoExtraArgs onto cargoTestExtraArgs in their wrapper,
  # so without dedup `--no-default-features` (and friends) end up
  # listed twice and cargo-schnee's clap rejects duplicates.  Order-
  # preserving so positional pairs like `--features X` are intact.
  dedup = xs:
    builtins.foldl' (acc: x: if lib.elem x acc then acc else acc ++ [ x ])
      [] xs;

  forwarded = removeAttrs args [
    "testScope" "cargoTestExtraArgs" "testRunnerArgs"
  ];

  built = self.lib.buildPackage (forwarded // {
    inherit package;
    intent = "test";
    cargoExtraArgs = dedup ((args.cargoExtraArgs or []) ++ cargoArgs);
  });

  runnerArgsStr = lib.escapeShellArgs testRunnerArgs;

  src = args.src;
  pkgArg = if package != null then "/${package}" else "";

in
  pkgs.runCommand "${built.name}-result" {
    inherit built src;
    passthru = { inherit built; };
    nativeBuildInputs = [ pkgs.binutils ];
  } ''
    set -euo pipefail
    mkdir -p $out

    # cargo-schnee bakes `/tmp/_schnee_md_<hash>` into each test
    # binary as `CARGO_MANIFEST_DIR`.  At compile time the symlink
    # points at the project-src store path so proc macros can read
    # files; at test runtime cargo-schnee's CLI re-creates it to
    # point at the writable source dir.  Replicate that here so
    # tests using `env!("CARGO_MANIFEST_DIR").join("testdata/...")`
    # find their fixtures.  Extract the symlink path from the
    # binary itself rather than recomputing the hash — keeps the
    # runner agnostic to cargo-schnee's hash scheme.
    found=0
    for bin in "$built"/bin/*; do
      [ -x "$bin" ] || continue
      found=1
      # cargo-schnee bakes `/tmp/_schnee_md_<hex>` into TestCompile
      # binaries as CARGO_MANIFEST_DIR.  Match precisely; rust
      # binaries don't null-separate string-table entries, so a bare
      # grep picks up trailing garbage from preceding strings.
      symlink_path="$(strings "$bin" | grep -oE '/tmp/_schnee_md_[0-9a-f]+' | head -1 || true)"
      if [ -n "$symlink_path" ]; then
        # Best-effort: point at $src/<package> if that exists, else
        # at $src.  Tests using `env!(CARGO_MANIFEST_DIR).join(…)`
        # resolve through this symlink at runtime.  Tests requiring
        # workspace-root-prefixed paths need the consumer to lay
        # out src to match (skeptiva does — `crates/` is its src).
        target="$src"
        if [ -n "${pkgArg}" ] && [ -d "$src${pkgArg}" ]; then
          target="$src${pkgArg}"
        fi
        ln -sfn "$target" "$symlink_path"
      fi
      echo "Running $(basename "$bin")..."
      "$bin" ${runnerArgsStr}
    done

    if [ "$found" = 0 ]; then
      echo "no test binaries found in $built/bin" >&2
      exit 1
    fi

    touch $out/ok
  ''
