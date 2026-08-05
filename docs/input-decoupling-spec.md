# Implementation spec: per-unit input decoupling

## Goal

Make every emitted unit derivation's `inputSrcs` contain **exactly** the
content that unit reads — its own crate source, the FODs of its dependency
closure, and any generated artefacts it consumes — and nothing else. The
target invariant:

> A unit's (hence a check's) `drvPath` changes **iff** a source file it reads, a
> compiler flag, a dependency's content, or a generated input it consumes
> changes.

With that invariant the consumer's CI matrix collapses to exactly the
transitively-affected checks, instead of rebuilding the world on every commit.

This spec aims straight at that end state. It is four changes; three are in
cargo-schnee, one is consumer-side.

## Current coupling (why everything re-keys)

Two monolithic inputs are baked into every unit, plus a planner that re-runs on
any edit. References are to the current tree.

1. **One `project-src` NAR.** `add_project_source_to_store` (`src/main.rs:1103`)
   serialises the entire git tree into a single CA store path. `mod.rs` passes
   that one `src_str` to every `construct_derivation` call (`src/plan_nix/mod.rs:1213`).
   Each unit's rustc script references files under it, and
   `collect_store_paths(&script, …)` (`src/plan_nix/derivation.rs:265`) sweeps
   the `project-src` root into the unit's `inputSrcs`. So a one-byte edit to any
   file under the source tree rehashes the NAR → every local unit's `drvPath`
   moves.

2. **One aggregate vendor dir.** `pkgs.rustPlatform.importCargoLock`
   (`nix/buildPackage.nix:136`) unpacks and **copies** every crate into a single
   output. Units insert that aggregate root wholesale into `inputSrcs`
   (`src/plan_nix/derivation.rs:254-255`) and resolve vendored paths to it
   (`map_to_store_path`, `src/plan_nix/unit_graph.rs:611-631`). Any `Cargo.lock`
   change re-keys the aggregate → every dependency unit's `drvPath` moves, even
   for crates whose own content is unchanged.

3. **Whole-tree planner.** The unit graph is generated from the full source
   tree, so the planner derivation (and its re-emitted dynamic unit drvs)
   churns on every edit even when the graph is identical.

`map_to_store_path` (`unit_graph.rs:617-624`) is where both monolithic roots are
pinned: source paths kept under `src_str`, vendored paths kept under
`vendor_str`. That function and `construct_derivation`'s `inputSrcs` assembly are
the two edit sites that recur below.

## Change 1 — per-crate source NARs

**Now:** one `src_store` threaded to all units; `source_file`/`manifest_dir` on
each `NixUnit` are `{src_store}/{crate_rel}/…` (`unit_graph.rs:192,197`).

**Target:** each local unit references a CA NAR of *only its own crate subtree*
(`manifest_dir`). Editing crate B leaves crate A's units byte-identical.

**Mechanism**

1. Add `add_crate_source_to_store(project_dir, crate_rel, files) -> store_path`
   beside `add_project_source_to_store` (`main.rs:1103`). It serialises just the
   `crate_rel` subtree (the slice of `collect_git_files` under it, plus that
   crate's `[package.metadata.schnee].extra-includes`) via the existing
   `nar::serialize_nar` / `nar::compute_nar_store_path` path. Outputs are CA, so
   identical crate content dedups across runs and across crates.

2. In `mod.rs`, compute a per-unit `src_store` keyed by the unit's crate
   (`manifest_dir` → `crate_rel` → per-crate store path; memoise per crate).
   Reuse the prefix-rewrite already present for cross-run cache reuse
   (`mod.rs:710-720`, which rewrites `source_file`/`manifest_dir` from
   `old_src_store` to `src_str`): generalise it to rewrite each unit's source
   paths from the whole-tree prefix to that unit's per-crate store. Pass the
   per-unit `src_store` into `construct_derivation` (`derivation.rs:57`) instead
   of one global value.

3. Adjust the remap. `--remap-path-prefix` (`derivation.rs:456-471`) currently
   maps the `project-src` root to `"crates"` (or the configured
   `sourceRootPrefix`). With a per-crate store, map `{crate_store}` →
   `{crate_rel}` (e.g. `crates/app-backend`) so diagnostics and debuginfo
   stay repo-relative. The `path_prefix_remaps` `(src_relative, replacement)`
   mechanism already supports this; only the replacement string changes.

4. `collect_store_paths` then captures the per-crate root from the script
   unchanged — no edit needed there.

**Edge cases**

- **Cross-crate file reads** (`include!("../other/…")`, a source file pulling a
  sibling crate's file): the per-crate NAR won't contain it. These fail loudly
  ("source file not found") rather than miscompile. Handle via the crate's
  `extra-includes`; document the escape hatch. Sweep the repo for
  `include!`/`include_str!`/`include_bytes!`/`path = "../` before rollout.
- **Workspace-root crate** (`manifest_dir == project root`) and the `.parent`
  handling for outside extra-includes (`derivation.rs:871-912`): include that
  crate's own root files in its NAR; keep `.parent` content only in the NARs of
  crates that actually use it.
- **`original_manifest_dir`** for `TestCompile` (`mod.rs:755-762`): keep pointed
  at the correct per-crate store after slicing.
- **External path-dep crates** (copied into `project-src` today via
  `find_external_path_deps`, `main.rs:652`/`1244-1261`): treat each as its own
  crate and give it its own per-crate NAR, same as workspace members.

**Acceptance:** edit a `.rs` body in crate X; the unit `drvPath`s of an
unrelated crate Y are **bit-identical** before/after. X's units (and content-
affected dependents) move; nothing else does.

## Change 4 — structure-keyed planner

**Now:** the planner reads the full source tree, so it re-runs and re-emits the
unit-drv set on every body edit.

**Target:** the planner's input is manifests + `Cargo.lock` + the source-file
*path set* — never file *contents*. A `.rs` body edit then does not re-run the
planner or perturb the emitted unit-drv set; only Change-1's per-crate NAR for
the edited crate moves.

**Mechanism**

The cargo unit graph is a function of `Cargo.toml` (all members), `Cargo.lock`,
features/flags, and which target files *exist* (autobins/autotests discover by
file presence) — not of `.rs` contents, and build scripts do not run at plan
time. So build the planner against a **skeleton tree**: every `Cargo.toml` and
`Cargo.lock` verbatim, every other source path present as a zero-byte file.
Generate this skeleton NAR (a sibling of `add_project_source_to_store`), run the
existing planner (`extract_units_from_bcx`, `mod.rs:620`) against it, then feed
Change-1's real per-crate NARs to the unit build derivations.

The skeleton's hash changes only when manifests, the lock, or the set of source
paths change — exactly the conditions under which the unit graph could differ.

**Acceptance:** edit a `.rs` body (no new/removed files); the planner
derivation's `drvPath` is unchanged. Add/remove a source file or edit a
manifest; it changes.

**Risk to verify first:** confirm cargo's unit-graph output is independent of
`.rs` *contents* for this workspace (it should be; cfg/features come from
manifests + flags). A single experiment — generate the graph from the real tree
and from the skeleton, diff — settles it. If any crate's graph differs, fall
back to including that crate's real source in the planner input.

## Change 2 — per-crate FOD vendoring

**Now:** `importCargoLock` copies all crates into one aggregate; units depend on
the aggregate root (`derivation.rs:254-255`, `unit_graph.rs:622`).

**Target:** each vendored crate is its own unpacked CA derivation; a unit's
vendor `inputSrcs` are only the FODs of its **transitive dependency closure**.
A `Cargo.lock` bump re-keys only units whose closure contains a changed crate.

**Mechanism**

1. **Vendoring (`nix/buildPackage.nix`):** replace `importCargoLock` with a
   per-crate vendor — one unpacked derivation per lock entry (fetch tarball FOD
   → unpack → write `.cargo-checksum.json`), assembled into a **symlink farm**
   (crane's `vendorCargoDeps` shape). cargo follows the symlinks; the offline
   config (`mod.rs:543`, `directory = …`) points at the farm.

2. **Per-unit dependency-FOD `inputSrcs` (`derivation.rs`):** drop the
   unconditional `input_srcs.insert(vendor_store)` (lines 254-255). Instead,
   from the unit graph's dependency edges compute each unit's transitive
   dependency closure, map each dependency crate to its individual vendor FOD,
   and insert only those.

3. **Direct-FOD path rewrite:** so a unit never depends on the farm *root*
   (which moves on any lock change), rewrite the unit's vendored source paths
   from `{vendor_dir}/{crate}` to `{crate_fod}/…` — the same rewrite pattern
   Change 1 applies to source. Extend `map_to_store_path`
   (`unit_graph.rs:622-624`) to resolve a vendored path to its crate FOD via a
   `crate → fod` map built from the farm (readlink) or the lock. The farm is
   then needed only for cargo's *plan-time* resolution, not for the per-unit
   build derivations' inputs.

**Edge cases:** vendored-crate build scripts (`BuildScriptRun` dep units) and
linking units that read vendored `.lib`/source (the reason for lines 250-256)
must get their own crate's FOD plus its build-dep FODs, not the aggregate. Git
dependencies vendor differently from registry crates — handle both in the farm.

**Acceptance:** bump one dependency in `Cargo.lock`; the unit `drvPath`s of units
whose dependency closure excludes that crate are unchanged.

## Change 3 — generated artefacts as env-delivered derivations (consumer-side)

This one needs **no cargo-schnee change** — it uses the mechanism cargo-schnee
already has and the consumer already proves: `app-backend-schema` is a
standalone derivation handed to its consumer via `KYSELY_SCHEMA_PATH` +
`passthruEnv`, and that closure is injected **only into build-script-run
sandboxes** (`derivation.rs:258-263`, `passthru_closure`), never the source NAR.

Generalise it consumer-side: build `spec/`, the openrpc/tellback codegen inputs,
and the kysely schema each as their own derivation, delivered to only their
actual consumers via env + `passthruEnv`; remove them from the global
`extraSources` so they stop folding into every crate's source NAR. Rework the
consuming `build.rs` to locate inputs via env rather than `CARGO_MANIFEST_DIR`-
relative paths. Once Change 1 lands this is partly subsumed (a non-consumer's
per-crate NAR already excludes the spec), but the env handoff is the clean
decoupling and removes the build-script's dependency on tree layout.

## Explicitly excluded (even ignoring effort)

- **Depinfo-driven per-file source closures** (`rustc --emit=dep-info`, two
  pass). It only decouples *within* a crate (a crate's test file not re-keying
  its lib unit). Checks aggregate a crate's units, so it changes **which checks
  collapse: not at all**, while adding a second pass and dynamic-derivation
  complexity. Per-crate (Change 1) is where the curve that matters flattens.
- **rmeta / metadata-pipelining dependency edges.** Nix CA early-cutoff already
  delivers the equivalent benefit at the store layer (a byte-identical upstream
  rlib does not re-realise dependents); adding `.rmeta` units doubles the graph
  for zero cascade reduction.

## Sequencing and shared machinery

1. **Change 1 + Change 4 together** — both rework how local source reaches
   units and share the prefix-rewrite plumbing (`mod.rs:710-720`); the skeleton
   planner is what makes Change 1's per-crate NARs the *only* thing that moves
   on a body edit. Biggest win, one coherent patch.
2. **Change 3** (consumer-side) — independent; lands whenever, mostly subsumed
   by Change 1 but worth it for the build-script decoupling.
3. **Change 2** — the heaviest; a vendoring rework plus closure computation and
   path rewriting. Orthogonal axis (lock), needed for full collapse on dep
   bumps.

## Verification harness

The drvPath-stability experiment is the acceptance gate for every change; it is
cheap (pure eval, no builds). Standardise it as a test in cargo-schnee's
`tests/` against a multi-crate fixture workspace:

- **Change 1:** eval unit drvPaths; edit an unrelated crate's `.rs`; re-eval;
  assert the untouched crate's unit drvPaths are identical.
- **Change 4:** assert the planner drvPath is identical across a body edit and
  differs across a file add/manifest edit.
- **Change 2:** assert a single-dep `Cargo.lock` bump leaves non-dependent units'
  drvPaths identical.

Each is a before/after drvPath equality check — encode it so regressions are
caught, not re-litigated.

## Risks

- Per-crate slicing breaks crates that read outside their dir; mitigated by the
  loud failure mode + `extra-includes`, but must be swept for first.
- The skeleton planner assumes content-independent unit-graph generation; verify
  per-workspace before relying on it.
- The vendor rework changes external fetching; registry vs git deps and the
  offline-config interaction with a symlink farm need explicit testing (crane
  proves the shape works, but cargo-schnee's exact invocation must be checked).

## Implementation status

**Mechanisms — implemented and proven by deterministic unit tests** (run with
`cargo test --bin cargo-schnee`; no Nix needed):

- **Change 1 (source):** `nar::crate_source_store_path` slices a single crate's
  subtree into its own CA NAR. Test `nar::tests::per_crate_source_path_independent_of_siblings`
  proves a crate's per-crate input is byte-identical across a *sibling* crate
  edit, while the whole-tree NAR (today's single `project-src`) moves on any
  edit.
- **Change 4 (planner):** `nar::serialize_nar_skeleton` /
  `nar::skeleton_source_store_path` blank all non-manifest files. Test
  `nar::tests::skeleton_source_path_is_body_independent` proves the planner input
  is unchanged by a `.rs` body edit but moves on a manifest edit or a
  source-file add.
- **Change 2 (lock):** `unit_graph::per_crate_vendor_id` /
  `aggregate_vendor_id`. Test `unit_graph::tests::vendor_identity_decouples_per_crate`
  proves a crate's vendor identity is unchanged by a sibling lock bump, while the
  aggregate id (today's reference) moves on any entry change.

These three tests are the deterministic proof that each axis's decoupling
mechanism works.

**Change 1 — wired into the live derivation path and build-proven.**
`run_plan_nix` now calls `assign_per_crate_src_stores` (the extracted, injectable
slicing logic): each local compile/test/doc unit gets a per-crate NAR via
`add_to_nix_store`, with its `source_file`/`manifest_dir` rewritten onto it,
while BuildScriptRun units keep the whole-tree `src_str`. Proven at three
levels:

- *Wiring logic* — `plan_nix::slice_tests::assign_decouples_compile_units_keeps_build_scripts_whole`
  drives the real per-crate NAR addressing over a temp two-crate tree: a crate-b
  edit leaves crate-a's assigned store byte-identical; build-script and non-local
  units keep the whole tree and are not rewritten.
- *Build-correct (host mode)* — the integration fixtures build and run through
  the patched binary: `fixture_workspace_binaries` and `fixture_workspace_advanced`
  (the latter has a proc-macro + build script, confirming build-script crates
  still compile because their run units keep the whole tree). Run with
  `cargo test --test integration -- --ignored`.
- *Build-correct (nix mode)* — building a real monorepo check through the patched
  cargo-schnee (`nix build .#checks.x86_64-linux.build-skeptiva-ci-robot
  --override-input cargo-schnee path:tmp/cargo-schnee`) succeeds end-to-end,
  exercising the recursive-nix per-crate store-adds inside the planner
  derivation (built `…-skeptiva-ci-robot.drv`, `…-aggregator.drv`, exit 0).

The **decoupling** claim rests on the deterministic tests above (a crate's unit
references a per-crate store that is byte-identical across sibling edits), not on
a rebuild experiment: a "rebuild ci-robot after editing an unrelated crate →
0 recompiles" run is *confounded*, because the check's outer drvPath is stable
(flake-input-keyed, not source-keyed), so the second build is a cache hit
whether or not the inner units decoupled. A clean end-to-end decoupling signal
needs the emitted per-unit drvPaths captured before/after — left for the
acceptance gate; the wiring + primitive tests are the rigorous proof.

**Change 4 — superseded by Change 1, do not wire.** Change 1 slices per-crate
source *inside the planner from the real `--src`* (via `add_to_nix_store`), so a
skeleton `--src` would feed the slicing empty crate trees and break builds — the
two are incompatible as written. And Change 1 already collapses the matrix
(units cache-hit), so Change 4's only benefit — avoiding a cheap planner re-run
— does not justify the conflict. Recommend dropping Change 4. (To revive it,
Change 1 would have to take per-crate NARs as Nix-provided inputs rather than
slicing from `--src`, a larger redesign.)

**Change 2 — wired and build-proven.** `assign_per_crate_src_stores` also slices
vendored units. The vendor dir (`cargo-vendor-dir`) is a crane-style symlink
farm whose entries are per-crate content-addressed store paths, so the slicer
`canonicalize`s each entry to its real target rather than `add`-ing the symlink
(re-adding captures the symlink, whose target is unmounted in the compile
sandbox → `couldn't read .../src/lib.rs`). Proven by the deterministic
`assign_decouples_local_and_vendored_crates` test (a symlink-farm model where
bumping one vendored crate leaves a sibling's target byte-identical) and by
nix-mode builds of ci-robot and app-backend (the latter with a large
vendored closure including ring, rustls, wasmtime), exit 0.

**Change 3 — wired and build-proven (cargo-schnee + consumer).** cargo-schnee
side: a crate opts in via `[package.metadata.schnee] self-contained-build-script
= true`; `assign_per_crate_src_stores` then slices its local BuildScriptRun unit
to per-crate source instead of carrying the whole tree (deterministic test:
`assign_decouples_local_and_vendored_crates` index 5). Consumer side: the shared
`openrpc_codegen::import::resolve_spec_path` reads the spec from a per-crate
`OPENRPC_SPEC_DIR_<CRATE>` env (keyed on `CARGO_PKG_NAME` — a bare name would
redirect *other* crates' build scripts, since cargo-schnee forwards passthru env
into every build-script-run; this was caught when the telemetry rpc client
regenerated from the app spec). app-backend supplies
`OPENRPC_SPEC_DIR_APP_BACKEND = "${specSrc}/app"` via `passthruEnv` and is
marked self-contained. Proven end-to-end: `build-app-backend` exits 0 and its
build-script-run's `CARGO_MANIFEST_DIR` is a per-crate `…-app-backend` store
(only build.rs/Cargo.toml/migrations/src — no sibling crates); the only other
source input is the spec tree, not the `crates/` workspace. So the unit no
longer re-keys on unrelated crate edits.

Net: all three coordinated changes are wired into the live pipeline and proven
by deterministic tests plus real nix builds; Change 4 is dropped. Remaining
polish (not required for the collapse): `OPENRPC_SPEC_DIR` points at the whole
`specSrc`, so a *spec* edit in an unrelated crate's spec still re-keys
app-backend's build-script-run — scope it to `spec/app` to remove that
minor over-inclusion. The build-script-run output is content-addressed, so even
then the expensive compile units cache-hit via CA early-cutoff.

## End-to-end acceptance gate

Once the integration is wired, prove it against the consumer monorepo (the
environment the change exists to serve). This is the RED→GREEN gate; it
reproduces the coupling today and must pass after wiring:

```sh
# In the monorepo, with the patched cargo-schnee pinned:
DRV() { nix eval --raw ".#checks.x86_64-linux.$1.drvPath"; }

before_a=$(DRV build-skeptiva-formatter)   # unrelated leaf crate
# edit an unrelated crate's source body, e.g. app-backend:
echo "// touch" >> crates/app-backend/src/main.rs
after_a=$(DRV build-skeptiva-formatter)
git checkout -- crates/app-backend/src/main.rs

test "$before_a" = "$after_a"  # FAILS today (coupling); MUST hold after wiring
```

Mirror it for the lock axis (bump one dependency in `crates/Cargo.lock`, assert a
non-dependent check's drvPath is unchanged) and the planner (a body edit leaves
the planner derivation's drvPath unchanged). Encode all three as a checked
script so the invariant is regression-tested, not re-litigated.
