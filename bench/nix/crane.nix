# Crane two-phase build: buildDepsOnly (dependencies) + buildPackage (source).
#
# On an incremental build the deps derivation hash is unchanged (same
# Cargo.lock ⇒ same dummy source ⇒ same derivation) so its output is
# cached from the clean build.  Only the source-phase derivation reruns.
{ pkgs, craneLib, justSrc, justSrcModified }:

let
  commonArgs = {
    pname = "just";
    version = "1.40.0";
    doCheck = false;

    # Disable crane's cargo-check pre-pass in buildDepsOnly.
    # By default crane runs `cargo check` before `cargo build` in the
    # deps phase, which adds overhead on clean builds (two cargo passes
    # instead of one).  The pre-pass benefits subsequent pipeline steps
    # (clippy, nextest) by pre-caching proc-macro and build-script
    # artifacts, but for a build-time-only benchmark it's pure overhead.
    # Disabling it is fair: it makes crane faster, not slower.
    cargoCheckCommand = "true";
  };

  # --- clean build ---
  # NOTE: We do NOT use craneLib.cleanCargoSource here because Just uses
  # include_str!("../CHANGELOG.md") which requires non-Rust files to be
  # present in the source tree.
  cleanArtifacts = craneLib.buildDepsOnly (commonArgs // {
    src = justSrc;
  });

  clean = craneLib.buildPackage (commonArgs // {
    src = justSrc;
    cargoArtifacts = cleanArtifacts;
  });

  # --- incremental build ---
  # buildDepsOnly uses a dummy source with only Cargo.{toml,lock}.
  # Since those are identical between justSrc and justSrcModified the deps
  # derivation hash is the same as cleanArtifacts — its output is already
  # in the store after the clean build.
  incrArtifacts = craneLib.buildDepsOnly (commonArgs // {
    src = justSrcModified;
  });

  incremental = craneLib.buildPackage (commonArgs // {
    src = justSrcModified;
    cargoArtifacts = incrArtifacts;
  });

in {
  inherit clean incremental;
}
