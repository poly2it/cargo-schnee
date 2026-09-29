# lib.buildDoc — rustdoc documentation via cargo-schnee.
#
# Thin wrapper around `lib.buildPackage` with `intent = "doc"`.  Maps
# the doc-specific options (`documentPrivateItems`, `noDeps`) to the
# corresponding cargo-schnee flags, then defers to buildPackage's
# planner + dyn-derivation chain.  buildPackage's install step
# auto-selects the doc layout (`$out/share/doc/`) for `intent = "doc"`.
{ self }:

{
  documentPrivateItems ? false,
  noDeps ? true,
  ...
}@args:

let
  inherit (args) pkgs;
  inherit (pkgs) lib;

  docFlags = lib.optionals noDeps [ "--no-deps" ]
    ++ lib.optionals documentPrivateItems [ "--document-private-items" ];

  forwarded = removeAttrs args [ "documentPrivateItems" "noDeps" ];

in
  self.lib.buildPackage (forwarded // {
    intent = "doc";
    cargoExtraArgs = (args.cargoExtraArgs or []) ++ docFlags;
  })
