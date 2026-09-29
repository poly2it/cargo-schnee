{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    cargo2nix = {
      url = "github:cargo2nix/cargo2nix";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };

    cargo-schnee = {
      url = "path:..";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };

    nixprof.url = "github:Kha/nixprof";
  };

  outputs = { self, nixpkgs, rust-overlay, crane, cargo2nix, cargo-schnee, nixprof }:
    let
      system = "x86_64-linux";
      lib = nixpkgs.lib;
      overlays = [ (import rust-overlay) ];
      pkgs = import nixpkgs { inherit system overlays; };

      rustToolchain = pkgs.rust-bin.stable.latest.default;

      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };

      # Upstream's buildPythonApplication
      # lacks the `pyproject` attribute required by current nixpkgs.
      # Also patches the report command for newer Nix's path-info JSON
      # format (dict keyed by path instead of list of objects).
      nixprofPkg = pkgs.python3Packages.buildPythonApplication {
        name = "nixprof";
        src = nixprof;
        pyproject = true;
        build-system = [ pkgs.python3Packages.setuptools ];
        dependencies = with pkgs.python3Packages; [
          networkx pydot click tabulate
        ];
        nativeBuildInputs = [ pkgs.moreutils ];
        propagatedBuildInputs = [ pkgs.moreutils ];
        postPatch = ''
          # Fix nix path-info --json format change (Nix 2.19+).
          # Old format: [{"path": "...", "references": [...]}]
          # New format: {"/nix/store/...": {"references": [...]}}
          substituteInPlace nixprof.py \
            --replace-fail \
              'for d in drv_data:' \
              'for d in ([{"path": k, **v} for k, v in drv_data.items()] if isinstance(drv_data, dict) else drv_data):'
        '';
      };

      benchTools = cargo-schnee.lib.buildPackage {
        inherit pkgs rustToolchain;
        src = ./tools;
        cargoLock = ./tools/Cargo.lock;
      };

      craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

      justSource = import ./nix/just-source.nix { inherit pkgs rustPlatform; };
      inherit (justSource) justSrc justSrcModified cargoVendoredDeps;

      brp = import ./nix/build-rust-package.nix {
        inherit pkgs rustPlatform justSrc justSrcModified cargoVendoredDeps;
      };

      craneBuild = import ./nix/crane.nix {
        inherit pkgs craneLib justSrc justSrcModified;
      };

      # cargo2nix is best-effort.  Its evaluation requires IFD (building the
      # cargo2nix CLI + running cargo2nix generate) which is very slow.
      # Disabled by default — set to true to enable.
      enableCargo2nix = true;
      cargo2nixEval =
        if enableCargo2nix
        then builtins.tryEval (import ./nix/cargo2nix.nix {
          inherit pkgs cargo2nix justSrc justSrcModified cargoVendoredDeps rustToolchain system;
        })
        else { success = false; };

      schneeBuild = import ./nix/cargo-schnee.nix {
        inherit pkgs justSrc justSrcModified rustToolchain;
        inherit cargo-schnee;
      };

      # We use builtins.unsafeDiscardStringContext on .drvPath to prevent
      # Nix from treating them as build-time dependencies.  Without this,
      # string-interpolating .drvPath causes Nix to realise (build) every
      # referenced derivation on the HOST, defeating the purpose of
      # benchmarking inside the VM.
      #
      # The host runner generates registration data (nix-store
      # --register-validity format) for all .drv files, source paths,
      # and tool outputs.  A systemd service in the VM loads this data
      # through the daemon before the benchmark starts.
      drv = path: builtins.unsafeDiscardStringContext path;

      brpCleanDrv        = drv brp.clean.drvPath;
      brpIncrDrv         = drv brp.incremental.drvPath;
      craneCleanDrv      = drv craneBuild.clean.drvPath;
      craneIncrDrv       = drv craneBuild.incremental.drvPath;
      schneeCleanDrv     = drv schneeBuild.clean.drvPath;
      schneeIncrDrv      = drv schneeBuild.incremental.drvPath;
      c2nCleanDrv        = if cargo2nixEval.success then drv cargo2nixEval.value.clean.drvPath else "";
      c2nIncrDrv         = if cargo2nixEval.success then drv cargo2nixEval.value.incremental.drvPath else "";

      benchScript = pkgs.writeShellScript "bench-runner" ''
        set -euo pipefail

        export PATH="${nixprofPkg}/bin:${pkgs.moreutils}/bin:${pkgs.time}/bin:${pkgs.nix}/bin:${rustToolchain}/bin:${pkgs.stdenv.cc}/bin:${pkgs.jq}/bin:${pkgs.coreutils}/bin:${pkgs.gnused}/bin:${pkgs.gawk}/bin:${pkgs.bash}/bin:${pkgs.git}/bin:${if cargo2nixEval.success then "${cargo2nix.packages.${system}.cargo2nix}/bin:" else ""}$PATH"
        export NIX_CONFIG="extra-experimental-features = flakes ca-derivations"

        RESULTS="/results/results.json"
        echo '[]' > "$RESULTS"

        # ── Filter handling ───────────────────────────────────────────────
        # The host runner may pass `--only <list>` which lands here as
        # `/results/bench-filter` (one comma-separated line). Empty / no
        # file means "run everything"; otherwise non-matching systems
        # are recorded as SKIPPED so the markdown table still renders.
        BENCH_FILTER=""
        if [ -f /results/bench-filter ]; then
          BENCH_FILTER="$(cat /results/bench-filter)"
        fi

        # Match `name` against the filter list. Returns 0 if `name` should
        # run, 1 otherwise. The shorthand `schnee` matches `cargo-schnee`.
        should_run() {
          local name="$1"
          if [ -z "$BENCH_FILTER" ]; then
            return 0
          fi
          local IFS=','
          for item in $BENCH_FILTER; do
            item="''${item## }"
            item="''${item%% }"
            if [ "$item" = "$name" ] || \
               { [ "$item" = "schnee" ] && [ "$name" = "cargo-schnee" ]; }; then
              return 0
            fi
          done
          return 1
        }

        add_result() {
          local sys="$1" scenario="$2" status="$3" duration="$4"
          local tmp
          tmp=$(mktemp)
          jq --arg s "$sys" --arg sc "$scenario" --arg st "$status" \
             --argjson d "$duration" \
             '. += [{"system": $s, "scenario": $sc, "status": $st, "duration_secs": $d}]' \
             "$RESULTS" > "$tmp" && mv "$tmp" "$RESULTS"
        }

        # Run a build command, time it, record the result.
        # Output goes to /results/build.log for post-mortem debugging.
        run_bench() {
          local sys="$1" scenario="$2"
          shift 2

          local start end rc duration
          start=$(date +%s%N)
          set +e
          "$@" >>/results/build.log 2>&1
          rc=$?
          set -e
          end=$(date +%s%N)
          duration=$(awk "BEGIN { printf \"%.1f\", ($end - $start) / 1000000000 }")

          if [ $rc -eq 0 ]; then
            add_result "$sys" "$scenario" "OK" "$duration"
            echo "    $scenario: $duration s"
          else
            add_result "$sys" "$scenario" "FAILED" "$duration"
            echo "    $scenario: FAILED ($duration s, exit $rc)"
          fi
        }

        drop_caches() {
          sync
          echo 3 > /proc/sys/vm/drop_caches
        }

        # Like run_bench, but also captures a nixprof profile for
        # nix-store --realise builds.
        run_bench_nix() {
          local sys="$1" scenario="$2" drv="$3"
          local profile="/results/profile-''${sys}-''${scenario}.log"

          local start end rc duration build_stderr
          build_stderr=$(mktemp)
          start=$(date +%s%N)
          set +e
          nixprof record -o "$profile" \
            nix-store --realise "$drv" >>/results/build.log 2>"$build_stderr"
          rc=$?
          set -e
          end=$(date +%s%N)
          duration=$(awk "BEGIN { printf \"%.1f\", ($end - $start) / 1000000000 }")

          # nixprof may not propagate the inner exit code.
          # Double-check by verifying the output path is valid.
          if [ $rc -eq 0 ]; then
            local first_out
            first_out=$(nix-store -q --outputs "$drv" 2>/dev/null | head -1)
            if [ -n "$first_out" ] && ! nix-store -q --hash "$first_out" >/dev/null 2>&1; then
              rc=1
              echo "    (nixprof reported success but output $first_out is not valid)" >>/results/build.log
            fi
          fi

          if [ $rc -ne 0 ]; then
            echo "    === build stderr for $sys/$scenario ===" >>/results/build.log
            cat "$build_stderr" >>/results/build.log
            echo "    === nix log $drv ===" >>/results/build.log
            nix log "$drv" >>/results/build.log 2>&1 || \
              nix-store -l "$drv" >>/results/build.log 2>&1 || true

            # Also extract logs of inner derivations that failed (e.g.
            # cargo-schnee's per-crate build-script-run drvs). nix-store
            # --realise reports "Cannot build '/nix/store/xxx.drv'" for
            # each one; try `nix log` on each so the post-mortem
            # contains the actual builder stderr (gcc errors, OOM kill
            # diagnostics, etc.) rather than just the cascading
            # "1 dependency failed".
            grep -oE "/nix/store/[a-z0-9]+-[^']*\.drv" "$build_stderr" \
              | sort -u | while IFS= read -r inner_drv; do
                # Skip the outer drv we already logged.
                if [ "$inner_drv" = "$drv" ]; then continue; fi
                echo "    === nix log $inner_drv ===" >>/results/build.log
                nix log "$inner_drv" >>/results/build.log 2>&1 || \
                  nix-store -l "$inner_drv" >>/results/build.log 2>&1 || \
                  echo "      (no log available)" >>/results/build.log
              done
          fi
          rm -f "$build_stderr"

          if [ $rc -eq 0 ]; then
            add_result "$sys" "$scenario" "OK" "$duration"
            echo "    $scenario: $duration s"
            # Generate critical-path report
            nixprof report -i "$profile" -p \
              > "/results/profile-''${sys}-''${scenario}-crit.txt" 2>&1 || true
            # Generate Chrome trace (viewable in perfetto.dev)
            nixprof report -i "$profile" \
              -c "/results/profile-''${sys}-''${scenario}.trace_event" \
              2>/dev/null || true
          else
            add_result "$sys" "$scenario" "FAILED" "$duration"
            echo "    $scenario: FAILED ($duration s, exit $rc)"
          fi
        }

        echo "============================================="
        echo "  cargo-schnee benchmark suite"
        echo "============================================="
        echo ""
        df -h / /nix/store 2>/dev/null || true
        echo ""

        # ── 1. cargo build (baseline) ────────────────────────────────────
        if should_run "cargo-build"; then
        echo ">>> [1/5] cargo build (baseline)"

        WORK=$(mktemp -d)
        cp -r ${justSrc}/* "$WORK/"
        chmod -R u+w "$WORK"

        # Set up vendored dependencies (no network in the VM).
        mkdir -p "$WORK/.cargo"
        cat > "$WORK/.cargo/config.toml" <<CARGO_EOF
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "${cargoVendoredDeps}"
CARGO_EOF

        cd "$WORK"
        drop_caches
        run_bench "cargo-build" "clean" cargo build --release

        # Incremental: modify src/lib.rs, reuse target/
        echo "" >> "$WORK/src/lib.rs"
        drop_caches
        run_bench "cargo-build" "incremental" cargo build --release

        cd /
        rm -rf "$WORK"
        echo ""
        else
          echo ">>> [1/5] cargo build (baseline) — SKIPPED (filter)"
          add_result "cargo-build" "clean" "SKIPPED" 0
          add_result "cargo-build" "incremental" "SKIPPED" 0
        fi

        # ── 2. cargo-schnee (derivation) ──────────────────────────────────
        if should_run "cargo-schnee"; then
        echo ">>> [2/5] cargo-schnee (derivation)"
        drop_caches
        run_bench_nix "cargo-schnee" "clean" ${schneeCleanDrv}

        drop_caches
        run_bench_nix "cargo-schnee" "incremental" ${schneeIncrDrv}
        echo ""
        else
          echo ">>> [2/5] cargo-schnee — SKIPPED (filter)"
          add_result "cargo-schnee" "clean" "SKIPPED" 0
          add_result "cargo-schnee" "incremental" "SKIPPED" 0
        fi

        # ── 3. buildRustPackage ──────────────────────────────────────────
        if should_run "buildRustPackage"; then
        echo ">>> [3/5] buildRustPackage"
        drop_caches
        run_bench_nix "buildRustPackage" "clean" ${brpCleanDrv}

        # Incremental: different source hash means new derivation
        drop_caches
        run_bench_nix "buildRustPackage" "incremental" ${brpIncrDrv}
        echo ""
        else
          echo ">>> [3/5] buildRustPackage — SKIPPED (filter)"
          add_result "buildRustPackage" "clean" "SKIPPED" 0
          add_result "buildRustPackage" "incremental" "SKIPPED" 0
        fi

        # ── 4. crane ────────────────────────────────────────────────────
        if should_run "crane"; then
        echo ">>> [4/5] crane"
        drop_caches
        run_bench_nix "crane" "clean" ${craneCleanDrv}

        # Incremental: deps phase cached (same Cargo.lock), source rebuilds
        drop_caches
        run_bench_nix "crane" "incremental" ${craneIncrDrv}
        echo ""
        else
          echo ">>> [4/5] crane — SKIPPED (filter)"
          add_result "crane" "clean" "SKIPPED" 0
          add_result "crane" "incremental" "SKIPPED" 0
        fi

        # ── 5. cargo2nix (best-effort) ──────────────────────────────────
        if should_run "cargo2nix"; then
        echo ">>> [5/5] cargo2nix"
        ${if cargo2nixEval.success then ''

        # Time the mandatory cargo2nix generate step (IFD cost).
        # This runs the cargo2nix CLI to produce Cargo.nix from Cargo.lock.
        C2N_WORK=$(mktemp -d)
        cp -r ${justSrc}/* "$C2N_WORK/"
        chmod -R u+w "$C2N_WORK"
        mkdir -p "$C2N_WORK/.cargo"
        cat > "$C2N_WORK/.cargo/config.toml" <<CARGO_EOF
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "${cargoVendoredDeps}"
CARGO_EOF
        cd "$C2N_WORK"
        git init -q
        git add -A
        git -c user.name=bench -c user.email=bench@localhost commit -q -m "init" --allow-empty
        drop_caches
        run_bench "cargo2nix" "generate" cargo2nix -o -l
        cd /
        rm -rf "$C2N_WORK"

        drop_caches
        run_bench_nix "cargo2nix" "clean" ${c2nCleanDrv}

        drop_caches
        run_bench_nix "cargo2nix" "incremental" ${c2nIncrDrv}
        '' else ''
        echo "    cargo2nix evaluation failed — recording SKIPPED"
        add_result "cargo2nix" "generate" "SKIPPED" 0
        add_result "cargo2nix" "clean" "SKIPPED" 0
        add_result "cargo2nix" "incremental" "SKIPPED" 0
        ''}
        else
          echo ">>> [5/5] cargo2nix — SKIPPED (filter)"
          add_result "cargo2nix" "generate" "SKIPPED" 0
          add_result "cargo2nix" "clean" "SKIPPED" 0
          add_result "cargo2nix" "incremental" "SKIPPED" 0
        fi
        echo ""

        # ── Done ────────────────────────────────────────────────────────
        echo "============================================="
        echo "  Benchmark complete"
        echo "============================================="
        echo ""
        jq . "$RESULTS"
        touch /results/.complete
        poweroff
      '';

      # Pre-built tool outputs that must be in the VM's store before any
      # benchmark runs.  These live on the read-only squashfs base image
      # and survive nix-collect-garbage.
      additionalStorePaths = [
        # Rust compiler and cargo
        rustToolchain

        # C toolchain (compiler, linker, glibc, binutils)
        pkgs.stdenv.cc
        pkgs.stdenv.cc.cc.lib

        # Nix daemon tools (needed for nix-store --realise, recursive-nix)
        # Include all outputs (doc, man) since multi-output .drv references
        # all outputs even when only "out" is needed.
        pkgs.nix
        pkgs.nix.doc
        pkgs.nix.man

        # Fixup tools used by buildRustPackage's install/fixup phases
        pkgs.patchelf
        pkgs.file

        # Source code and vendored deps
        justSrc
        justSrcModified
        cargoVendoredDeps

        # JSON processor (used by bench script)
        pkgs.jq

        # Profiling: nixprof + moreutils (ts) + GNU time
        nixprofPkg
        pkgs.moreutils
        pkgs.time

        # cargo2nix generation step (run inside VM)
        pkgs.git
      ]
      # cargo2nix CLI (conditional on evaluation success)
      ++ lib.optionals cargo2nixEval.success [
        cargo2nix.packages.${system}.cargo2nix
      ]
      # stdenv base tools: coreutils, gnused, gnugrep, gawk, gnutar, etc.
      ++ pkgs.stdenv.initialPath;

      nixos = nixpkgs.lib.nixosSystem {
        inherit system;
        specialArgs = { inherit benchScript additionalStorePaths; };
        modules = [ ./nix/vm.nix ];
      };

      hostRunner = pkgs.writeShellScriptBin "cargo-schnee-bench" ''
        set -euo pipefail

        RESULTS_DIR="/tmp/cargo-schnee-bench-results"

        # ── Argument parsing ──────────────────────────────────────────────
        # `--only <name>` (repeatable, comma-separated) restricts the
        # in-VM bench script to a subset of systems. Unmatched systems
        # are recorded as SKIPPED so the markdown table still renders.
        # Names accepted: cargo-build, cargo-schnee, buildRustPackage,
        # crane, cargo2nix. The shorthand `schnee` matches `cargo-schnee`.
        BENCH_FILTER=""
        while [ $# -gt 0 ]; do
          case "$1" in
            --only)
              if [ -z "''${2:-}" ]; then
                echo "ERROR: --only requires an argument" >&2
                exit 2
              fi
              BENCH_FILTER="$2"
              shift 2
              ;;
            --only=*)
              BENCH_FILTER="''${1#--only=}"
              shift
              ;;
            -h|--help)
              cat <<'EOF'
Usage: cargo-schnee-bench [--only <system[,system,...]>]

Options:
  --only <list>   Comma-separated subset of systems to actually run.
                  Others are recorded as SKIPPED. Examples:
                    --only schnee
                    --only cargo-schnee,crane
EOF
              exit 0
              ;;
            *)
              echo "ERROR: unknown argument: $1" >&2
              exit 2
              ;;
          esac
        done

        # Check for KVM support
        if [ ! -e /dev/kvm ]; then
          echo "WARNING: /dev/kvm not found — VM will run without hardware"
          echo "         virtualization and will be VERY slow."
          echo ""
        fi

        # Prepare results directory
        rm -rf "$RESULTS_DIR"
        mkdir -p "$RESULTS_DIR"

        # Hand the filter to the in-VM bench script via the 9p mount.
        # Empty file means "run everything"; anything else is read by
        # `should_run` inside the bench script.
        if [ -n "$BENCH_FILTER" ]; then
          echo "$BENCH_FILTER" > "$RESULTS_DIR/bench-filter"
          echo "Bench filter: $BENCH_FILTER"
          echo ""
        fi

        # The VM's Nix DB (via closureInfo/additionalPaths) only knows
        # about runtime closures of pre-built tools.  Build-time paths
        # — .drv files, source paths (builder.sh, setup scripts), and
        # tool outputs (cargo hooks, stdenv phases) — must be registered
        # separately.
        #
        # We compute the .drv closure, pre-build DIRECT input deps of
        # the benchmark derivations, then generate --load-db format
        # registration data with 0 references (avoids reference
        # validation issues with not-yet-built output paths that .drv
        # files reference).  The VM loads this in boot.postBootCommands
        # BEFORE the nix-daemon starts, so the daemon sees all paths.

        echo "Computing .drv dependency closure..."
        CLOSURE=$(mktemp)
        ${pkgs.nix}/bin/nix-store -qR \
          ${brpCleanDrv} ${brpIncrDrv} \
          ${craneCleanDrv} ${craneIncrDrv} \
          ${schneeCleanDrv} ${schneeIncrDrv} \
          ${if cargo2nixEval.success then "${c2nCleanDrv} ${c2nIncrDrv}" else ""} \
          | awk '!seen[$0]++' > "$CLOSURE"
        CLOSURE_COUNT=$(wc -l < "$CLOSURE")
        echo "Found $CLOSURE_COUNT paths in .drv closure."

        # Pre-build source tarballs and tool outputs needed by the
        # benchmark derivations.  The VM has no network, so all
        # fixed-output derivations (source fetches) must be pre-built
        # on the host.
        #
        # Strategy:
        #  1. Build ALL .tar.gz FODs from the drv closure (source fetches).
        #  2. Build direct tool/hook input deps of the seed drvs.
        #  3. Skip compilation artifacts (crate-*, just-*, *-deps-*).
        echo "Pre-building source tarballs (FODs)..."
        FETCHED=0
        grep '\.tar\.gz\.drv$' "$CLOSURE" | while IFS= read -r drv; do
          out=$(${pkgs.nix}/bin/nix-store -q --outputs "$drv" 2>/dev/null | head -1)
          if [ -n "$out" ] && [ ! -e "$out" ]; then
            name=$(basename "$drv" .drv | sed 's/^[a-z0-9]*-//')
            echo "  Fetching: $name"
            ${pkgs.nix}/bin/nix-store --realise "$drv" >/dev/null 2>&1 || \
              echo "  WARNING: failed to fetch $name"
          fi
        done
        FETCHED=$(grep '\.tar\.gz\.drv$' "$CLOSURE" | while IFS= read -r drv; do
          out=$(${pkgs.nix}/bin/nix-store -q --outputs "$drv" 2>/dev/null | head -1)
          [ -n "$out" ] && [ -e "$out" ] && echo "$out"
        done | wc -l)
        echo "Source tarballs available: $FETCHED"

        echo "Pre-building direct input deps (tools only)..."
        SEED_DRVS="${brpCleanDrv} ${brpIncrDrv} ${craneCleanDrv} ${craneIncrDrv} ${schneeCleanDrv} ${schneeIncrDrv} ${if cargo2nixEval.success then "${c2nCleanDrv} ${c2nIncrDrv}" else ""}"
        BUILT=0
        for seed_drv in $SEED_DRVS; do
          [ -f "$seed_drv" ] || continue
          for input_drv in $(${pkgs.nix}/bin/nix-store -q --references "$seed_drv" | grep '\.drv$'); do
            name=$(basename "$input_drv" .drv | sed 's/^[a-z0-9]*-//')
            # Skip compilation artifacts; keep crate source tarballs
            case "$name" in
              just-*|*-deps-*) continue ;;
              crate-*) [[ "$name" != *.tar.gz ]] && continue ;;
            esac
            for out in $(${pkgs.nix}/bin/nix-store -q --outputs "$input_drv" 2>/dev/null); do
              if [ ! -e "$out" ]; then
                echo "  Building: $name"
                ${pkgs.nix}/bin/nix-store --realise "$input_drv" >/dev/null 2>&1 || \
                  echo "  WARNING: failed to build $name"
                BUILT=$((BUILT + 1))
                break
              fi
            done
          done
        done
        echo "Pre-built $BUILT missing deps."

        # Collect existing output paths from the .drv closure.
        # These include source tarballs (fixed-output derivation outputs) and
        # tool outputs needed for builds.  Benchmark TARGET outputs don't exist
        # on the host (they only exist inside the VM).
        #
        # IMPORTANT: Exclude compilation artifacts that happen to be built on
        # the host during flake evaluation (e.g. crane's buildDepsOnly).
        # Including them would give that build system an unfair advantage.
        echo "Collecting existing outputs..."
        DRV_FILES=$(mktemp)
        grep '\.drv$' "$CLOSURE" > "$DRV_FILES" || true
        EXISTING_OUTPUTS=$(mktemp)
        xargs -r ${pkgs.nix}/bin/nix-store -q --outputs < "$DRV_FILES" 2>/dev/null \
          | sort -u | while IFS= read -r p; do
            if [ -e "$p" ]; then
              name=$(basename "$p" | sed 's/^[a-z0-9]*-//')
              case "$name" in
                just-deps-*|just-1.40.0) echo "  Excluding compilation artifact: $name" >&2 ;;
                crate-*)
                  if [[ "$name" != *.tar.gz ]]; then
                    echo "  Excluding compilation artifact: $name" >&2
                  else
                    echo "$p"
                  fi ;;

                *) echo "$p" ;;
              esac
            fi
          done > "$EXISTING_OUTPUTS" || true
        rm -f "$DRV_FILES"
        OUTPUT_COUNT=$(wc -l < "$EXISTING_OUTPUTS")
        echo "Found $OUTPUT_COUNT existing outputs (after excluding artifacts)."

        # Combine .drv closure + existing outputs, dedup.
        # Note: the VM's boot.postBootCommands also cleans host-leaked
        # compilation outputs from the Nix DB, so host-side filtering
        # here is defense-in-depth only.
        ALL_PATHS=$(mktemp)
        cat "$CLOSURE" "$EXISTING_OUTPUTS" | awk '!seen[$0]++' > "$ALL_PATHS"
        rm -f "$CLOSURE" "$EXISTING_OUTPUTS"
        TOTAL_COUNT=$(wc -l < "$ALL_PATHS")
        echo "Total paths to register: $TOTAL_COUNT"

        # Generate --load-db format with 0 references.
        # Using 0 references avoids reference validation — .drv files
        # reference their own (not-yet-built) output paths, which would
        # fail validation.  Nix reads .drv files directly for build
        # input resolution; DB references are only used for GC.
        echo "Generating registration data (load-db, 0 refs)..."
        PATH_INFO_JSON=$(mktemp)
        xargs -r -n100 ${pkgs.nix}/bin/nix path-info --json \
          < "$ALL_PATHS" 2>/dev/null \
          | ${pkgs.jq}/bin/jq -s 'add // {}' > "$PATH_INFO_JSON"
        rm -f "$ALL_PATHS"

        # Generate --load-db format matching closureInfo's registration format:
        #   path, narHash, narSize, deriver (or empty), numRefs, refs...
        # nix path-info --json returns {"/nix/store/xxx": {narHash, narSize, deriver, ...}}
        REG_ENTRIES=$(${pkgs.jq}/bin/jq 'length' "$PATH_INFO_JSON")
        ${pkgs.jq}/bin/jq -r '
          to_entries[] | select(.value != null) |
          "\(.key)\n\(.value.narHash)\n\(.value.narSize)\n\(.value.deriver // "")\n0"
        ' "$PATH_INFO_JSON" > "$RESULTS_DIR/store-reg.txt"
        rm -f "$PATH_INFO_JSON"
        echo "Registration entries: $REG_ENTRIES paths"
        echo ""

        # Delete any existing qcow2 disk image to ensure a clean slate.
        # Previous runs leave outputs on the writable overlay that would
        # make nix-store --realise skip building (defeating the benchmark).
        rm -f nixos.qcow2

        echo "Starting benchmark VM..."
        echo "This will take a while (expect 30-90 minutes with KVM)."
        echo ""

        # Launch the VM.  It will poweroff after benchmarks complete.
        ${nixos.config.system.build.vm}/bin/run-*-vm -nographic </dev/null || true

        echo ""

        # Check completion
        if [ ! -f "$RESULTS_DIR/.complete" ]; then
          echo "ERROR: Benchmarks did not complete successfully."
          if [ -f "$RESULTS_DIR/results.json" ]; then
            echo "Partial results:"
            ${pkgs.jq}/bin/jq . "$RESULTS_DIR/results.json"
          fi
          exit 1
        fi

        # Generate BENCHMARK.md in current working directory (where nix run was invoked)
        ${benchTools}/bin/bench-tools generate-markdown \
          "$RESULTS_DIR/results.json" > BENCHMARK.md

        # Also copy raw results and profiles
        cp "$RESULTS_DIR/results.json" results.json
        mkdir -p profiles
        cp "$RESULTS_DIR"/profile-*.trace_event profiles/ 2>/dev/null || true
        cp "$RESULTS_DIR"/profile-*-crit.txt profiles/ 2>/dev/null || true
        cp "$RESULTS_DIR"/profile-*.log profiles/ 2>/dev/null || true
        # cargo-schnee's planner spans (one file per variant), emitted
        # via CARGO_SCHNEE_TRACE — see bench/nix/cargo-schnee.nix.
        cp "$RESULTS_DIR"/schnee-planner-*.trace_event profiles/ 2>/dev/null || true

        # Combined per-system stderr from inside the VM. The bench script
        # appends every build's stdout/stderr to /results/build.log, so
        # this is the place to look when a system FAILED — outer
        # `cargo-schnee` panics, OOM kills, etc. land here.
        cp "$RESULTS_DIR/build.log" build.log 2>/dev/null || true

        echo "Results written to:"
        echo "  $(pwd)/BENCHMARK.md"
        echo "  $(pwd)/results.json"
        echo "  $(pwd)/profiles/   (nixprof logs + traces)"
        echo ""
        cat BENCHMARK.md

        # Per-derivation timing breakdown from raw nixprof logs
        echo ""
        ${benchTools}/bin/bench-tools parse-profiles "$RESULTS_DIR"
      '';

    in {
      packages.${system} = {
        vm = nixos.config.system.build.vm;
        default = nixos.config.system.build.vm;
      };

      apps.${system}.default = {
        type = "app";
        program = "${hostRunner}/bin/cargo-schnee-bench";
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ jq gawk coreutils gnused bash ];
      };
    };
}
