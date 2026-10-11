# Vendors `lockFile` the way `lib.buildPackage` does for `cargoLock`. Builds
# that share a lockfile should bind the result once and pass it as `cargoDeps`,
# because each `cargoLock` build vendors the lockfile again.
{
  pkgs,
  lockFile,
  # Hashes keyed `"<name>-<version>"` for git dependencies that should be
  # fixed-output fetches.
  outputHashes ? {},
}:

pkgs.rustPlatform.importCargoLock {
  inherit lockFile outputHashes;
  # `Cargo.lock` records only a commit for a git source, so it is fetched with
  # `builtins.fetchGit` by that commit instead of needing an `outputHashes` entry.
  allowBuiltinFetchGit = true;
  # crates.io rejects the `curl` User-Agent that `fetchurl` sends, see
  # rust-lang/crates.io#13482. `static.crates.io` serves the same archives at
  # the same paths without that check, so recorded hashes still match.
  extraRegistries = {
    "https://github.com/rust-lang/crates.io-index" =
      "https://static.crates.io/crates";
  };
}
