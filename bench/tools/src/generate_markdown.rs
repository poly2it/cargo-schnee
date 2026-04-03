use serde::Deserialize;
use std::fmt::Write;
use std::fs;

#[derive(Deserialize)]
struct Result {
    system: String,
    scenario: String,
    status: String,
    duration_secs: Option<f64>,
}

const SYSTEMS: &[(&str, &str)] = &[
    ("cargo-build", "cargo build"),
    ("cargo-schnee", "cargo-schnee"),
    ("buildRustPackage", "buildRustPackage"),
    ("crane", "crane"),
    ("cargo2nix", "cargo2nix"),
];

fn get<'a>(results: &'a [Result], system: &str, scenario: &str) -> Option<&'a Result> {
    results
        .iter()
        .find(|r| r.system == system && r.scenario == scenario)
}

fn get_duration(results: &[Result], system: &str, scenario: &str) -> Option<f64> {
    get(results, system, scenario)
        .filter(|r| r.status == "OK")
        .and_then(|r| r.duration_secs)
}

fn fmt_duration(secs: f64) -> String {
    format!("{secs:.1}s")
}

fn fmt_ratio(a: f64, b: f64) -> String {
    if b == 0.0 {
        "---".to_string()
    } else {
        format!("{:.1}x", a / b)
    }
}

fn row(cols: &[&str], bold: bool) -> String {
    if bold {
        let bolded: Vec<String> = cols.iter().map(|c| format!("**{c}**")).collect();
        format!("| {} |", bolded.join(" | "))
    } else {
        format!("| {} |", cols.join(" | "))
    }
}

pub fn run(path: &str) {
    print!("{}", generate(path));
}

pub fn generate(path: &str) -> String {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read {path}: {e}");
            std::process::exit(1);
        }
    };
    let results: Vec<Result> = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Invalid JSON in {path}: {e}");
            std::process::exit(1);
        }
    };
    let mut out = String::new();

    let cargo_clean = get_duration(&results, "cargo-build", "clean").unwrap_or(0.0);
    let cargo_incr = get_duration(&results, "cargo-build", "incremental").unwrap_or(0.0);

    // cargo2nix generation step.
    let c2n_gen = get(&results, "cargo2nix", "generate");
    let c2n_gen_ok = c2n_gen.is_some_and(|r| r.status == "OK");
    let c2n_gen_duration = c2n_gen.and_then(|r| r.duration_secs).unwrap_or(0.0);
    let c2n_clean_duration = get_duration(&results, "cargo2nix", "clean").unwrap_or(0.0);
    let c2n_total_clean = if c2n_gen_ok {
        c2n_gen_duration + c2n_clean_duration
    } else {
        c2n_clean_duration
    };

    // Find best non-baseline clean build time.
    let best_clean = SYSTEMS
        .iter()
        .filter(|(sys, _)| *sys != "cargo-build")
        .filter_map(|(sys, _)| get_duration(&results, sys, "clean"))
        .reduce(f64::min);

    // Find best non-baseline incremental build time.
    let best_incr = SYSTEMS
        .iter()
        .filter(|(sys, _)| *sys != "cargo-build")
        .filter_map(|(sys, _)| get_duration(&results, sys, "incremental"))
        .reduce(f64::min);

    // Header.
    write!(
        out,
        "\
# Comparative benchmark

## Just

Build of [Just](https://github.com/casey/just) version 1.40.0. The incremental change is an appended newline to `lib.rs` and `src/main.rs`.

### Clean build

| Build system | Time | vs. cargo build |
|---|---|---|
"
    )
    .unwrap();

    // Clean build table.
    for &(sys, label) in SYSTEMS {
        let Some(duration) = get_duration(&results, sys, "clean") else {
            writeln!(out, "| {label} | --- | --- |").unwrap();
            continue;
        };
        if sys == "cargo-build" {
            writeln!(
                out,
                "{}",
                row(&[label, &fmt_duration(duration), "baseline"], false)
            )
            .unwrap();
        } else if sys == "cargo2nix" {
            if c2n_gen_ok {
                writeln!(
                    out,
                    "{}",
                    row(
                        &[
                            label,
                            &fmt_duration(c2n_total_clean),
                            &fmt_ratio(c2n_total_clean, cargo_clean),
                        ],
                        false,
                    )
                )
                .unwrap();
                let is_best = best_clean == Some(duration);
                let pregen_label = format!("{label} (w/pregeneration)");
                writeln!(
                    out,
                    "{}",
                    row(
                        &[
                            &pregen_label,
                            &fmt_duration(duration),
                            &fmt_ratio(duration, cargo_clean),
                        ],
                        is_best,
                    )
                )
                .unwrap();
            } else {
                let is_best = best_clean == Some(duration);
                writeln!(
                    out,
                    "{}",
                    row(
                        &[label, &fmt_duration(duration), &fmt_ratio(duration, cargo_clean)],
                        is_best,
                    )
                )
                .unwrap();
            }
        } else {
            let is_best = best_clean == Some(duration);
            writeln!(
                out,
                "{}",
                row(
                    &[label, &fmt_duration(duration), &fmt_ratio(duration, cargo_clean)],
                    is_best,
                )
            )
            .unwrap();
        }
    }

    // Incremental build table.
    write!(
        out,
        "
### Incremental build

| Build system | Time | vs. cargo build | vs. clean |
|---|---|---|---|
"
    )
    .unwrap();

    for &(sys, label) in SYSTEMS {
        let clean_duration = get_duration(&results, sys, "clean").unwrap_or(0.0);
        let Some(incr_duration) = get_duration(&results, sys, "incremental") else {
            writeln!(out, "| {label} | --- | --- | --- |").unwrap();
            continue;
        };
        let speedup = fmt_ratio(incr_duration, clean_duration);
        if sys == "cargo-build" {
            writeln!(
                out,
                "{}",
                row(
                    &[label, &fmt_duration(incr_duration), "baseline", &speedup],
                    false,
                )
            )
            .unwrap();
        } else {
            let is_best = best_incr == Some(incr_duration);
            writeln!(
                out,
                "{}",
                row(
                    &[
                        label,
                        &fmt_duration(incr_duration),
                        &fmt_ratio(incr_duration, cargo_incr),
                        &speedup,
                    ],
                    is_best,
                )
            )
            .unwrap();
        }
    }

    // Static footer.
    write!(
        out,
        "
## Build system descriptions

| System | Strategy | Incremental behavior |
|---|---|---|
| cargo build | Invokes the cargo crate to compute a build graph directly. | Invokes the cargo crate to compute which artefacts are dirty. Uses timestamps internally. |
| cargo-schnee | Invokes the cargo crate to compute a build graph directly. | Uses composite keys for build configuration updates and compilation identities for translation units. Uses content-addressed derivations everywhere. Uses Nix's dirty detection for source files. Enables granularity down to translation unit level. |
| buildRustPackage | Invokes cargo to perform a full build. | Does not have an incremental strategy, rebuilds on any source change. |
| crane | Builds are split into two phases. The first derivation builds the dependencies, which can be shared in a workspace. The second derivation builds a discrete artefact. | The dependency phase is cached when Cargo.lock is unchanged; the source phase rebuilds entirely. |
| cargo2nix | Generates Nix expressions from Cargo.lock using cargo crate for dependency resolution, then builds using Nix derivations. | Rebuilds only dirty crates from the pregenerated Nix expression. Enables granularity down to crate level. |

<details>
<summary>Methodology</summary>

- All benchmarks run in a NixOS QEMU VM with 16 GiB of RAM, 4 cores, and 30 GiB of disk. A Fresh VM disk image is created per run.
- The benchmarks are designed to not count potential overhead of building prerequisites of the build system itself. As such, toolchains, sources and `.drv` files are pre-populated via 9p host store sharing.
- Disk caches are dropped via `echo 3 > /proc/sys/vm/drop_caches` before every timed operation.
- The VM has no network access. All source tarballs and tools are pre-populated.
- For `cargo build`, the target directory is deleted before the clean build but preserved between clean and incremental to test native incrementality.
- For Nix-based systems, `nix-store --realise <drv>` is timed. The incremental build benefits from cached outputs of the clean build, since no garbage collection runs between the clean and incremental builds within the same system.
- cargo2nix requires running `cargo2nix generate` to produce a `Cargo.nix` file before builds can start. The \"cargo2nix\" row in the clean build table includes this generation cost; \"cargo2nix (w/pregeneration)\" shows the build-only time. The incremental table omits generation since it only runs once per `Cargo.lock` change.
- crane's default `cargo check` pre-pass in `buildDepsOnly` is disabled via `cargoCheckCommand = \"true\"`. This pre-pass adds overhead on clean builds by running two cargo passes, and only benefits subsequent pipeline steps like clippy or nextest, not the build itself. Disabling it makes crane faster, not slower. Since crane's optimisation strategy revolves caching the build dependencies, realistic usage of crane would rarely invoke the first phase or its check pre-pass.

</details>
"
    )
    .unwrap();

    out
}
