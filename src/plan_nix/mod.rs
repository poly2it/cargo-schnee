//! The `plan-nix` subcommand: runs inside the Nix planner derivation.
//!
//! 1. Calls `create_bcx()` to extract the unit graph — NO compilation.
//! 2. Constructs rustc commands directly from unit metadata.
//! 3. Build scripts become their own derivations (compile + run).
//! 4. Adds each derivation via `nix derivation add` (topological order).
//! 5. Prints the resulting root `.drv` content to stdout.

mod aterm;
mod daemon;
mod derivation;
mod derivation_format;
mod unit_graph;
pub(crate) mod util;

use aterm::{collect_drv_refs, compute_drv_store_path, serialize_derivation_aterm};
use daemon::NixDaemonConn;
use derivation::{
    construct_derivation, downstream_placeholder, nix_derivation_add, nix_store_closure,
    self_placeholder,
};
use unit_graph::{compute_topo_levels, extract_units_from_bcx};
use util::{
    find_cross_linker, find_sysroot_rlib, which_clippy_driver, which_command,
    which_command_no_deref, which_rustc, which_rustdoc,
};

use anyhow::{Context, Result};
use cargo::core::Workspace;
use cargo::core::compiler::UnitInterner;
use cargo::ops::{self, CompileOptions};
use cargo::util::command_prelude::UserIntent;
use cargo::util::context::GlobalContext;
use tracing::info;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Build configuration
// ---------------------------------------------------------------------------

pub struct ProfileConfig {
    pub name: String,
    pub opt_level: &'static str,
    pub debug_info: bool,
}

impl ProfileConfig {
    pub fn dev() -> Self {
        Self {
            name: "dev".into(),
            opt_level: "0",
            debug_info: true,
        }
    }
    pub fn release() -> Self {
        Self {
            name: "release".into(),
            opt_level: "3",
            debug_info: false,
        }
    }
}

pub struct TargetConfig {
    pub host_triple: String,
    pub target_triple: String,
    /// Nix system string for the build machine (e.g. "x86_64-linux").
    /// All derivations use this as their `system` — even cross-compiled ones,
    /// because the builder always runs on the host.
    pub nix_system: String,
}

impl TargetConfig {
    pub fn native() -> Self {
        let arch = std::env::consts::ARCH;
        Self {
            host_triple: format!("{}-unknown-linux-gnu", arch),
            target_triple: format!("{}-unknown-linux-gnu", arch),
            nix_system: format!("{}-linux", arch),
        }
    }

    pub fn with_target(target_triple: &str) -> Self {
        let host_arch = std::env::consts::ARCH;
        Self {
            host_triple: format!("{}-unknown-linux-gnu", host_arch),
            target_triple: target_triple.to_string(),
            nix_system: format!("{}-linux", host_arch),
        }
    }

    pub fn is_cross(&self) -> bool {
        self.host_triple != self.target_triple
    }

    pub fn is_msvc(&self) -> bool {
        self.target_triple.contains("msvc")
    }

    pub fn is_windows(&self) -> bool {
        self.target_triple.contains("windows")
    }

    /// Map the target architecture to Microsoft's notation for Windows SDK paths.
    /// Returns None for non-Windows targets.
    pub fn ms_arch(&self) -> Option<&'static str> {
        if !self.is_windows() {
            return None;
        }
        let arch = self.target_triple.split('-').next().unwrap_or("");
        Some(match arch {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "i686" | "i586" => "x86",
            _ => "x64",
        })
    }
}

/// Extract CARGO_CFG_* env vars from `rustc --print cfg` output for a target.
/// This matches cargo's own env var generation for build scripts.
fn extract_cfg_envs(cfgs: &[cargo_platform::Cfg]) -> Vec<(String, String)> {
    use std::collections::BTreeMap;
    let mut cfg_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for cfg in cfgs {
        match cfg {
            cargo_platform::Cfg::Name(n) => {
                let name = n.to_string();
                if name == "debug_assertions" {
                    continue;
                }
                cfg_map.entry(name).or_default();
            }
            cargo_platform::Cfg::KeyPair(k, v) => {
                cfg_map.entry(k.to_string()).or_default().push(v.clone());
            }
        }
    }
    cfg_map
        .into_iter()
        .map(|(k, v)| {
            let key = format!("CARGO_CFG_{}", k.to_uppercase().replace('-', "_"));
            (key, v.join(","))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Unit representation
// ---------------------------------------------------------------------------

/// The kind of derivation a NixUnit represents.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum UnitKind {
    /// Regular rustc compilation (lib, bin, proc-macro, etc.)
    Compile,
    /// Metadata-only check (--emit=metadata, no codegen)
    Check,
    /// Documentation generation via rustdoc
    Doc,
    /// Compilation with --test (test/bench harness)
    TestCompile,
    /// Compilation of a build script binary
    BuildScriptCompile,
    /// Execution of a compiled build script
    BuildScriptRun,
}

/// A unit in the generated Nix DAG.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NixUnit {
    pub(crate) key: String,
    pub(crate) drv_name: String,
    pub(crate) kind: UnitKind,
    /// Source file path (nix store path)
    pub(crate) source_file: String,
    pub(crate) crate_name: String,
    pub(crate) crate_types: Vec<String>,
    pub(crate) edition: String,
    /// Enabled features → --cfg feature="X"
    pub(crate) features: Vec<String>,
    /// (extern_crate_name, dep_unit_key) — direct deps
    pub(crate) dep_extern: Vec<(String, String)>,
    /// All transitive dep keys (for -L dependency=)
    pub(crate) all_dep_keys: Vec<String>,
    /// Key of the build-script-run derivation this unit depends on (if any)
    pub(crate) build_script_dep: Option<String>,
    /// Key of the build-script-compile derivation (for BuildScriptRun units)
    pub(crate) build_script_compile_key: Option<String>,
    /// CARGO_MANIFEST_DIR for the package (mapped to nix store path)
    pub(crate) manifest_dir: String,
    /// Original (pre-mapping) manifest dir — the writable project path.
    /// For TestCompile units, this is used as CARGO_MANIFEST_DIR so that
    /// compile-time `env!("CARGO_MANIFEST_DIR")` captures a writable path.
    #[serde(default)]
    pub(crate) original_manifest_dir: String,
    /// Standard cargo env vars for build scripts
    pub(crate) cargo_envs: Vec<(String, String)>,
    /// Deterministic hash for -C extra-filename and -C metadata
    pub(crate) extra_filename: String,
    /// Whether this unit needs a linker (proc-macro, bin, cdylib, dylib)
    pub(crate) needs_linker: bool,
    /// Whether this is a local (project) crate vs a dependency
    pub(crate) is_local: bool,
    /// The package's `links` value (e.g. "openssl" for openssl-sys)
    pub(crate) links: Option<String>,
    /// Keys of other BuildScriptRun units this depends on, with their links name.
    pub(crate) links_dep_keys: Vec<(String, String)>,
    /// Whether this is a root unit (binary target requested by the user)
    #[serde(default)]
    pub(crate) is_root: bool,
    /// Binary target name from cargo (e.g. "just", "bin-a")
    #[serde(default)]
    pub(crate) target_name: String,
    /// Whether this unit compiles for the host (build scripts, proc-macros)
    /// vs the target. For native builds host == target so this is irrelevant.
    #[serde(default)]
    pub(crate) for_host: bool,
    /// Filled after nix derivation add
    pub(crate) drv_path: Option<String>,
}

impl NixUnit {
    /// Clear the drv_path (for cache serialization — drv_paths are recomputed each run).
    pub fn clear_drv_path(&mut self) {
        self.drv_path = None;
    }

    /// Compute the output filename that rustc will produce for this unit.
    ///
    /// For `--extern` linking rustc needs the `.rlib` (which contains a
    /// `.rustc` metadata section), not the `.so`.  Crates that declare
    /// `crate-type = ["cdylib", "rlib"]` produce both; we must pick the rlib.
    /// Proc-macros are the exception — they are loaded as shared objects.
    pub(crate) fn output_lib_filename(&self) -> String {
        // Check mode emits only .rmeta (no .rlib/.so)
        if self.kind == UnitKind::Check {
            return format!("lib{}{}.rmeta", self.crate_name, self.extra_filename);
        }
        // Doc mode outputs HTML directories, not linkable artifacts.
        // This shouldn't be called for Doc units, but return a sentinel.
        if self.kind == UnitKind::Doc {
            return format!("doc/{}", self.crate_name);
        }
        if self.crate_types.iter().any(|ct| ct == "bin")
            || self.kind == UnitKind::BuildScriptCompile
        {
            format!("{}{}", self.crate_name, self.extra_filename)
        } else if self.crate_types.iter().any(|ct| ct == "proc-macro") {
            format!("lib{}{}.so", self.crate_name, self.extra_filename)
        } else if self
            .crate_types
            .iter()
            .any(|ct| ct == "rlib" || ct == "lib")
        {
            format!("lib{}{}.rlib", self.crate_name, self.extra_filename)
        } else if self
            .crate_types
            .iter()
            .any(|ct| ct == "dylib" || ct == "cdylib")
        {
            format!("lib{}{}.so", self.crate_name, self.extra_filename)
        } else {
            format!("lib{}{}.rlib", self.crate_name, self.extra_filename)
        }
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Return drv_paths for local Compile and BuildScriptCompile units.
/// Used to replay cached diagnostics after a warm build.
pub fn local_compile_drv_paths(units: &[NixUnit]) -> Vec<&str> {
    units
        .iter()
        .filter(|u| {
            u.is_local
                && matches!(
                    u.kind,
                    UnitKind::Compile
                        | UnitKind::Check
                        | UnitKind::Doc
                        | UnitKind::TestCompile
                        | UnitKind::BuildScriptCompile
                )
        })
        .filter_map(|u| u.drv_path.as_deref())
        .collect()
}

/// Read custom sys-env mappings from `[workspace.metadata.schnee.sys-env]` or
/// `[package.metadata.schnee.sys-env]` in the given Cargo.toml.
/// Returns a list of (links_name, env_var_name) pairs.
fn read_custom_sys_env(manifest_path: &Path) -> Vec<(String, String)> {
    let content = match std::fs::read_to_string(manifest_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let doc: toml::Value = match toml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    // Try workspace.metadata.schnee.sys-env first, then package.metadata.schnee.sys-env
    let table = doc
        .get("workspace")
        .and_then(|w| w.get("metadata"))
        .and_then(|m| m.get("schnee"))
        .and_then(|s| s.get("sys-env"))
        .and_then(|v| v.as_table())
        .or_else(|| {
            doc.get("package")
                .and_then(|p| p.get("metadata"))
                .and_then(|m| m.get("schnee"))
                .and_then(|s| s.get("sys-env"))
                .and_then(|v| v.as_table())
        });
    match table {
        Some(t) => {
            let mut result: Vec<(String, String)> = t
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect();
            result.sort();
            result
        }
        None => Vec::new(),
    }
}

/// Check whether system libraries required by -sys crates are discoverable via
/// pkg-config.  Emits warnings for missing libraries — never errors, since some
/// -sys crates bundle their native code and don't need external packages.
fn check_system_libraries(
    nix_units: &[NixUnit],
    pkg_config_bin: &Option<String>,
    pkg_config_path_env: &str,
) {
    let pc_bin = match pkg_config_bin {
        Some(bin) => bin,
        None => return, // can't validate without pkg-config
    };

    // Collect unique (links_name, pkg_name) pairs from -sys crates.
    let mut seen = std::collections::HashSet::new();
    let mut checks: Vec<(&str, &str)> = Vec::new();
    for u in nix_units {
        if let Some(ref links) = u.links
            && seen.insert(links.as_str())
        {
            let pkg_name = u
                .cargo_envs
                .iter()
                .find(|(k, _)| k == "CARGO_PKG_NAME")
                .map(|(_, v)| v.as_str())
                .unwrap_or(&u.crate_name);
            checks.push((links.as_str(), pkg_name));
        }
    }

    for &(links_name, pkg_name) in &checks {
        let ok = std::process::Command::new(pc_bin)
            .arg("--exists")
            .arg(links_name)
            .env("PKG_CONFIG_PATH", pkg_config_path_env)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !ok {
            tracing::warn!(
                "System library '{}' (needed by {}) not found via pkg-config. \
                 Add the corresponding package to buildInputs to make it available.",
                links_name,
                pkg_name,
            );
        }
    }
}

/// Run the plan-nix pipeline: extract unit graph, add derivations, emit root .drv.
///
/// Derivation `.drv` paths are computed in-process (ATerm serialization + store path hash).
/// New derivations are registered via the Nix daemon Unix socket (no process spawning).
/// If `verify_drv_paths` is true, also runs `nix derivation add` and compares paths.
///
/// `closure_cache` maps nix store paths to their transitive closures (`nix-store -qR`).
/// On cache hit, the closure query is skipped. New results are inserted into the map
/// so the caller can persist them.
///
/// `cached_units` allows skipping the expensive `create_bcx()` call on cache hit.
/// If `Some((old_src_store, units))`, path prefixes are fixed up and units are used directly.
/// Returns `(root_drv_path, units_for_caching)`.
/// Per-unit data needed for derivation registration in a topological
/// level. Computed once per level so the batched validity probe and the
/// possibly-parallel writes both work from the same struct.
struct LevelUnit {
    /// Index into the outer `nix_units` slice.
    i: usize,
    /// Copy of `nix_units[i].key`. Embedded so worker threads can produce
    /// useful diagnostics without sharing access to the outer slice.
    unit_key: String,
    json: serde_json::Value,
    drv_file_name: String,
    aterm: Vec<u8>,
    refs: Vec<String>,
    drv_path: String,
}

fn ensure_path_match(observed: &str, unit: &LevelUnit) -> Result<()> {
    if observed != unit.drv_path {
        anyhow::bail!(
            "Path mismatch for {}:\n  computed: {}\n  observed: {}",
            unit.unit_key,
            unit.drv_path,
            observed,
        );
    }
    Ok(())
}

/// Register a single unit by adding its `.drv` ATerm bytes via the
/// daemon, with one reconnect attempt on transient failure, falling
/// through to `nix derivation add` (CLI) when the daemon is unreachable.
///
/// `conn` reflects the *connection state for the current chunk*: a
/// successful reconnect updates it in place; a permanent failure clears
/// it so subsequent calls skip the daemon path entirely. Either way the
/// returned path is verified against the in-process computation.
fn register_unit(conn: &mut Option<NixDaemonConn>, unit: &LevelUnit) -> Result<String> {
    let refs: Vec<&str> = unit.refs.iter().map(|s| s.as_str()).collect();

    if let Some(c) = conn.as_mut() {
        match c.add_text_to_store(&unit.drv_file_name, &unit.aterm, &refs) {
            Ok(p) => {
                ensure_path_match(&p, unit)?;
                return Ok(p);
            }
            Err(e) => {
                info!(
                    "Daemon error for {}: {}, attempting reconnect",
                    unit.unit_key, e,
                );
            }
        }
        // First connection failed; try a fresh one.
        match NixDaemonConn::connect() {
            Ok(mut new_c) => match new_c.add_text_to_store(&unit.drv_file_name, &unit.aterm, &refs)
            {
                Ok(p) => {
                    ensure_path_match(&p, unit)?;
                    *conn = Some(new_c);
                    return Ok(p);
                }
                Err(e2) => {
                    info!(
                        "Reconnected daemon also failed for {}: {}, falling back to CLI",
                        unit.unit_key, e2,
                    );
                    *conn = None;
                }
            },
            Err(e2) => {
                info!(
                    "Daemon reconnect failed for {}: {}, falling back to CLI",
                    unit.unit_key, e2,
                );
                *conn = None;
            }
        }
    }

    let p = nix_derivation_add(&unit.json)
        .with_context(|| format!("Failed to add derivation for {}", unit.unit_key))?;
    ensure_path_match(&p, unit)?;
    Ok(p)
}

/// Distribute `units` round-robin across `n` chunks. Returns exactly
/// `n` Vecs, possibly empty for over-provisioned cases. Round-robin
/// keeps per-chunk wall time roughly even across workers when unit
/// build cost is correlated with index — which it tends to be after
/// `compute_topo_levels` orders dependencies before dependents.
fn chunk_round_robin<T>(units: Vec<T>, n: usize) -> Vec<Vec<T>> {
    let mut chunks: Vec<Vec<T>> = (0..n).map(|_| Vec::new()).collect();
    for (idx, u) in units.into_iter().enumerate() {
        chunks[idx % n].push(u);
    }
    chunks
}

/// Bootstrap-only unit-graph extraction.
///
/// Loads the workspace via cargo-as-library and extracts the `Vec<NixUnit>`
/// plus target/host `cfg` env tables. No closure queries, no derivation
/// registration, no `nix-store --realise`. Used both as the cache-miss
/// fallback inside [`run_plan_nix`] and as the body of the
/// `cargo-schnee compute-graph` subcommand that pre-computes a graph for
/// `nix/buildPackage.nix` to feed back via `CARGO_SCHNEE_UNIT_GRAPH`.
///
/// Side effects: sets `CARGO_HOME` to a tempdir, briefly changes the
/// process CWD to `src` for `create_bcx`, restores it on exit. Both are
/// existing behaviours preserved verbatim from the previous inline code.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub fn fresh_unit_graph(
    src: &Path,
    vendor_dir: &Path,
    profile: &ProfileConfig,
    target: &TargetConfig,
    user_intent: UserIntent,
    packages: &[String],
    exclude: &[String],
    features: &[String],
    no_default_features: bool,
    all_targets: bool,
) -> Result<(Vec<NixUnit>, Vec<(String, String)>, Vec<(String, String)>)> {
    let manifest_path = src.join("Cargo.toml");
    if !manifest_path.exists() {
        anyhow::bail!("No Cargo.toml found at {}", manifest_path.display());
    }

    // Write cargo config for vendored sources (unique temp dirs for concurrent safety)
    let cargo_home_tmp = tempfile::Builder::new()
        .prefix("cargo-schnee-home-")
        .tempdir()?;
    let cargo_home = cargo_home_tmp.path().to_path_buf();
    std::fs::write(
        cargo_home.join("config.toml"),
        format!(
            "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"{}\"\n",
            vendor_dir.display()
        ),
    )?;
    unsafe { std::env::set_var("CARGO_HOME", &cargo_home) };

    // Change CWD to the nix store source so cargo's config discovery
    // (which walks up from CWD) doesn't find .cargo/config.toml files
    // left by build tools (e.g. cargoSetupPostUnpackHook in /build/).
    let old_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(src).context("Failed to cd to source dir for create_bcx")?;

    let target_tmp = tempfile::Builder::new()
        .prefix("cargo-schnee-target-")
        .tempdir()?;
    let target_dir = target_tmp.path().to_path_buf();

    let mut gctx = GlobalContext::default()?;
    gctx.configure(
        0,
        false,
        None,
        // frozen: false — when building a workspace subset (e.g. -p foo), the
        // Cargo.lock may contain entries for sibling crates that aren't part of
        // this resolve.  frozen=true would reject the lock as "out of date".
        // This is safe because: (1) the Nix sandbox prevents network access so
        // no new crates can be fetched, and (2) deps are pre-vendored so the
        // resolve is fully offline regardless.
        false,
        true,
        true,
        &Some(target_dir.to_string_lossy().to_string().into()),
        &[],
        &[],
    )?;

    let ws = Workspace::new(&manifest_path, &gctx)?;
    let mut options = CompileOptions::new(&gctx, user_intent)?;
    // Set profile if not dev
    if profile.name != "dev" {
        options.build_config.requested_profile =
            cargo::util::interning::InternedString::new(&profile.name);
    }
    // Set cross-compilation target if specified
    if target.is_cross() {
        options.build_config.requested_kinds =
            vec![cargo::core::compiler::CompileKind::Target(
                cargo::core::compiler::CompileTarget::new(&target.target_triple)?,
            )];
    }
    // Package selection: -p/--package narrows the build to specific crates,
    // --exclude removes crates from the default workspace set.
    if !packages.is_empty() {
        options.spec = ops::Packages::Packages(packages.to_vec());
    } else if !exclude.is_empty() {
        options.spec = ops::Packages::OptOut(exclude.to_vec());
    } else if ws.is_virtual() {
        options.spec = ops::Packages::All(Vec::new());
    }

    // Feature flags
    if !features.is_empty() || no_default_features {
        options.cli_features = cargo::core::resolver::CliFeatures::from_command_line(
            features,
            false, // all_features
            !no_default_features,
        )?;
    }

    // --all-targets: extend cargo's default target set (lib + bins) to
    // include tests, examples, and benches.  Mirrors `cargo --all-targets`.
    if all_targets {
        options.filter = ops::CompileFilter::new_all_targets();
    }

    // Extract unit graph and target cfg — NO compilation happens here
    let interner = UnitInterner::new();
    let bcx = ops::create_bcx(&ws, &options, &interner, None)?;
    let units = extract_units_from_bcx(&bcx, &bcx.roots, src, vendor_dir, user_intent)?;
    let bcx_cfg_envs =
        extract_cfg_envs(bcx.target_data.cfg(bcx.build_config.requested_kinds[0]));
    let bcx_host_cfg_envs = if target.is_cross() {
        extract_cfg_envs(
            bcx.target_data
                .cfg(cargo::core::compiler::CompileKind::Host),
        )
    } else {
        bcx_cfg_envs.clone()
    };
    drop(bcx);
    info!("Extracted {} units from unit graph", units.len());

    // Restore CWD
    if let Some(cwd) = old_cwd {
        let _ = std::env::set_current_dir(cwd);
    }

    Ok((units, bcx_cfg_envs, bcx_host_cfg_envs))
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub fn run_plan_nix(
    src: &Path,
    vendor_dir: &Path,
    verify_drv_paths: bool,
    closure_cache: &mut HashMap<String, Vec<String>>,
    cached_units: Option<(String, Vec<NixUnit>)>,
    cached_cfg_envs: Option<Vec<(String, String)>>,
    cached_host_cfg_envs: Option<Vec<(String, String)>>,
    profile: &ProfileConfig,
    target: &TargetConfig,
    user_intent: UserIntent,
    packages: &[String],
    exclude: &[String],
    features: &[String],
    no_default_features: bool,
    passthru_envs: &[(String, String)],
    project_dir: Option<&Path>,
    document_private_items: bool,
    // Run clippy-driver instead of rustc on local (workspace) compile units.
    // Dependency units are unchanged so per-unit derivations stay shared with
    // regular check / build runs.
    clippy: bool,
    // Lint args forwarded to clippy-driver on every local clippy unit.
    // Empty when `clippy` is false or when the caller did not pass any
    // post-`--` driver flags.
    clippy_lint_args: &[String],
    // `--remap-path-prefix` rules forwarded to every compile unit.  Each
    // `(src_relative, replacement)` is resolved against `src_store` inside
    // `build_compile_script`, so callers express remaps in terms of the
    // project-src layout without knowing the content-addressed hash.
    path_prefix_remaps: &[(String, String)],
    // Number of parallel daemon connections to use for derivation
    // registration. `None` defaults to the number of available CPU
    // cores; `Some(1)` reproduces the pre-parallel behaviour. Capped
    // per topo level by the level's width so registration of small
    // levels does not over-allocate connections.
    registration_jobs: Option<usize>,
    // Mirror of `cargo --all-targets`: when true, plan tests, examples
    // and benches alongside the default lib + bins.  Used by
    // `cargo schnee clippy --all-targets` so lint coverage extends to
    // test modules and example crates.
    all_targets: bool,
) -> Result<(
    Vec<(String, String, UnitKind)>,
    Vec<NixUnit>,
    Vec<(String, String)>,
    Vec<(String, String)>,
)> {
    let _root_span = tracing::info_span!("plan_nix").entered();

    let manifest_path = src.join("Cargo.toml");
    if !manifest_path.exists() {
        anyhow::bail!("No Cargo.toml found at {}", manifest_path.display());
    }

    // Read custom sys-env overrides from [workspace.metadata.schnee.sys-env]
    // or [package.metadata.schnee.sys-env] in the root Cargo.toml.
    let custom_sys_env = read_custom_sys_env(&manifest_path);

    let src_str = src.to_string_lossy().to_string();

    let _extract_span = tracing::info_span!(
        "extract_units",
        cached = cached_units.is_some(),
        crate_count = tracing::field::Empty,
    )
    .entered();
    let (mut nix_units, cfg_envs, host_cfg_envs) = if let Some((old_src_store, mut units)) =
        cached_units
    {
        // Fix up source paths: replace old src_store prefix with current one
        if old_src_store != src_str {
            for unit in &mut units {
                if unit.source_file.starts_with(&old_src_store) {
                    unit.source_file =
                        format!("{}{}", src_str, &unit.source_file[old_src_store.len()..]);
                }
                if unit.manifest_dir.starts_with(&old_src_store) {
                    unit.manifest_dir =
                        format!("{}{}", src_str, &unit.manifest_dir[old_src_store.len()..]);
                }
            }
        }
        // Clear drv_path from cached units (will be recomputed)
        for unit in &mut units {
            unit.drv_path = None;
        }
        let cfg = cached_cfg_envs.unwrap_or_default();
        let host_cfg = cached_host_cfg_envs.unwrap_or_default();
        info!(
            "Using cached unit graph ({} units, {} cfg envs, src fixup: {})",
            units.len(),
            cfg.len(),
            old_src_store != src_str
        );
        (units, cfg, host_cfg)
    } else {
        fresh_unit_graph(
            src,
            vendor_dir,
            profile,
            target,
            user_intent,
            packages,
            exclude,
            features,
            no_default_features,
            all_targets,
        )?
    };
    tracing::Span::current().record("crate_count", nix_units.len());
    drop(_extract_span);
    tracing::info!(units = nix_units.len(), "extract_units complete");

    // Populate original_manifest_dir for TestCompile units so that compile-time
    // env!("CARGO_MANIFEST_DIR") captures the writable project path instead of
    // the read-only nix store path.
    if let Some(proj) = project_dir {
        let proj_str = proj.to_string_lossy();
        for unit in &mut nix_units {
            if let Some(suffix) = unit.manifest_dir.strip_prefix(src_str.as_str()) {
                unit.original_manifest_dir = format!("{}{}", proj_str, suffix);
            }
        }
    }

    // Resolve tool paths
    let rustc_path = which_rustc()?;
    let rustc_str = rustc_path.to_string_lossy().to_string();
    let rustdoc_str = if user_intent.is_doc() {
        let rustdoc_path = which_rustdoc()?;
        rustdoc_path.to_string_lossy().to_string()
    } else {
        String::new()
    };
    // For clippy mode, resolve clippy-driver.  It is invoked as a rustc
    // replacement for local (workspace) units; dep units keep using rustc
    // so their per-unit derivations stay byte-identical to regular builds.
    let clippy_str = if clippy {
        let path = which_clippy_driver()?;
        path.to_string_lossy().to_string()
    } else {
        String::new()
    };

    // Query rustc for its sysroot — this works with wrapper scripts (nixpkgs'
    // rustc-wrapper) where the binary's store path differs from the sysroot.
    let rustc_sysroot = {
        let output = std::process::Command::new(&rustc_path)
            .arg("--print")
            .arg("sysroot")
            .output()
            .context("Failed to run rustc --print sysroot")?;
        anyhow::ensure!(
            output.status.success(),
            "rustc --print sysroot failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let sysroot = String::from_utf8(output.stdout)
            .context("rustc sysroot is not UTF-8")?
            .trim()
            .to_string();
        // Resolve symlinks so we get the real nix store path
        PathBuf::from(&sysroot)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&sysroot))
            .to_string_lossy()
            .to_string()
    };
    let rustc_store = rustc_sysroot;
    info!("Sysroot: {}", rustc_store);

    // Find the proc_macro rlib from the HOST sysroot (proc-macros always run on host)
    let host_sysroot_lib = PathBuf::from(&rustc_store)
        .join("lib/rustlib")
        .join(&target.host_triple)
        .join("lib");
    let proc_macro_rlib = find_sysroot_rlib(&host_sysroot_lib, "proc_macro")?;
    info!("proc_macro rlib: {}", proc_macro_rlib);
    // For cross-compilation, verify the target sysroot exists
    if target.is_cross() {
        let target_sysroot_lib = PathBuf::from(&rustc_store)
            .join("lib/rustlib")
            .join(&target.target_triple)
            .join("lib");
        if !target_sysroot_lib.exists() {
            anyhow::bail!(
                "Target sysroot not found at {}. \
                 Ensure your Rust toolchain includes the target: \
                 targets = [\"{}\"]",
                target_sysroot_lib.display(),
                target.target_triple,
            );
        }
    }
    let bash_path = which_command("bash")?.to_string_lossy().to_string();
    let mkdir_path = which_command_no_deref("mkdir")?
        .to_string_lossy()
        .to_string();
    let coreutils_store = PathBuf::from(&mkdir_path)
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Cannot derive store path from mkdir"))?
        .to_string_lossy()
        .to_string();

    let host_cc_path = which_command_no_deref("cc")?;
    let host_cc_bin_dir = host_cc_path
        .parent()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot derive bin dir from cc path: {}",
                host_cc_path.display()
            )
        })?
        .to_string_lossy()
        .to_string();
    let host_cc_store = host_cc_path
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Cannot derive store path from cc"))?
        .to_string_lossy()
        .to_string();

    // For cross-compilation, resolve a target-specific linker
    let (target_cc_bin_dir, target_cc_store) = if target.is_cross() {
        let cross_cc = find_cross_linker(&target.target_triple)?;
        let bin_dir = cross_cc
            .parent()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cannot derive bin dir from cross-linker: {}",
                    cross_cc.display()
                )
            })?
            .to_string_lossy()
            .to_string();
        let store = cross_cc
            .parent()
            .and_then(|p| p.parent())
            .ok_or_else(|| anyhow::anyhow!("Cannot derive store path from cross-linker"))?
            .to_string_lossy()
            .to_string();
        (bin_dir, store)
    } else {
        (host_cc_bin_dir.clone(), host_cc_store.clone())
    };

    // For MSVC cross-compilation, resolve Windows SDK paths from XWIN_DIR
    let (win_sdk_lib_dirs, win_sdk_store) = if target.is_msvc() {
        let xwin_dir = std::env::var("XWIN_DIR").map_err(|_| {
            anyhow::anyhow!(
                "XWIN_DIR environment variable not set. \
                 Point it to the pkgs.windows.sdk output \
                 (e.g. XWIN_DIR=${{pkgs.windows.sdk}} in your devShell)."
            )
        })?;
        let xwin_dir = PathBuf::from(&xwin_dir)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&xwin_dir));
        let ms_arch = target.ms_arch().unwrap_or("x64");
        let lib_dirs = vec![
            format!("{}/crt/lib/{}", xwin_dir.display(), ms_arch),
            format!("{}/sdk/lib/um/{}", xwin_dir.display(), ms_arch),
            format!("{}/sdk/lib/ucrt/{}", xwin_dir.display(), ms_arch),
        ];
        // Derive the Nix store root from the XWIN_DIR path
        let xwin_str = xwin_dir.to_string_lossy();
        let store = if let Some(after_prefix) = xwin_str.strip_prefix("/nix/store/")
            && let Some(end) = after_prefix.find('/')
        {
            format!("/nix/store/{}", &after_prefix[..end])
        } else {
            xwin_str.to_string()
        };
        (lib_dirs, Some(store))
    } else {
        (Vec::new(), None)
    };

    // Collect system build environment for -sys build scripts.
    let pkg_config_path_env = {
        let for_target = std::env::var("PKG_CONFIG_PATH_FOR_TARGET").unwrap_or_default();
        let base = std::env::var("PKG_CONFIG_PATH").unwrap_or_default();
        if for_target.is_empty() {
            base
        } else if base.is_empty() {
            for_target
        } else {
            format!("{}:{}", for_target, base)
        }
    };
    let pkg_config_bin = which_command_no_deref("pkg-config")
        .ok()
        .map(|p| p.to_string_lossy().to_string());

    // Collect all unique store paths that need closure queries.
    let mut closure_store_paths: Vec<String> = Vec::new();
    closure_store_paths.push(rustc_store.clone());
    closure_store_paths.push(host_cc_store.clone());
    // Capture clippy-driver's store closure when in clippy mode.  In typical
    // rust-overlay setups it lives inside the same toolchain symlink farm as
    // rustc and the closure is a no-op, but if clippy ships separately we
    // need its libs available in the per-unit sandbox.
    let clippy_store: Option<String> = if !clippy_str.is_empty() {
        let canon =
            std::fs::canonicalize(&clippy_str).unwrap_or_else(|_| PathBuf::from(&clippy_str));
        canon
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };
    if let Some(ref store) = clippy_store
        && !closure_store_paths.contains(store)
    {
        closure_store_paths.push(store.clone());
    }
    if target.is_cross() && target_cc_store != host_cc_store {
        closure_store_paths.push(target_cc_store.clone());
    }
    if let Some(ref sdk_store) = win_sdk_store
        && !closure_store_paths.contains(sdk_store)
    {
        closure_store_paths.push(sdk_store.clone());
    }
    // PKG_CONFIG_PATH entries
    let mut sys_store_roots: Vec<String> = Vec::new();
    for pkg_path in pkg_config_path_env.split(':').filter(|s| !s.is_empty()) {
        if let Some(after_prefix) = pkg_path.strip_prefix("/nix/store/")
            && let Some(end) = after_prefix.find('/')
        {
            let store_root = format!("/nix/store/{}", &after_prefix[..end]);
            if !sys_store_roots.contains(&store_root) {
                sys_store_roots.push(store_root.clone());
                closure_store_paths.push(store_root);
            }
        }
    }
    // pkg-config binary
    let pkg_config_store = pkg_config_bin.as_ref().and_then(|p| {
        PathBuf::from(p)
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_string_lossy().to_string())
    });
    if let Some(ref store) = pkg_config_store
        && !closure_store_paths.contains(store)
    {
        closure_store_paths.push(store.clone());
    }

    // passthruEnv values may reference store paths (e.g. LIBCLANG_PATH).
    // Their closures must be available in build-script-run sandboxes.
    let mut passthru_store_roots: Vec<String> = Vec::new();
    for (_name, value) in passthru_envs {
        for segment in value.split(':') {
            if let Some(after_prefix) = segment.strip_prefix("/nix/store/")
                && let Some(end) = after_prefix.find('/')
            {
                let store_root = format!("/nix/store/{}", &after_prefix[..end]);
                if !passthru_store_roots.contains(&store_root) {
                    passthru_store_roots.push(store_root.clone());
                    closure_store_paths.push(store_root);
                }
            } else if segment.starts_with("/nix/store/") {
                // Value is the store path itself, without a trailing subpath.
                let store_root = segment.to_string();
                if !passthru_store_roots.contains(&store_root) {
                    passthru_store_roots.push(store_root.clone());
                    closure_store_paths.push(store_root);
                }
            }
        }
    }

    // Deduplicate
    closure_store_paths.sort();
    closure_store_paths.dedup();

    // Separate cached vs uncached store paths
    let uncached: Vec<&str> = closure_store_paths
        .iter()
        .filter(|p| !closure_cache.contains_key(p.as_str()))
        .map(|p| p.as_str())
        .collect();

    let _closure_span = tracing::info_span!(
        "query_closures",
        total = closure_store_paths.len(),
        uncached = uncached.len(),
    )
    .entered();
    if !uncached.is_empty() {
        info!(
            "Querying {} tool closures ({} cached, {} to query)",
            closure_store_paths.len(),
            closure_store_paths.len() - uncached.len(),
            uncached.len()
        );

        // Query all uncached closures in parallel
        let results: Vec<(String, Result<Vec<String>>)> = std::thread::scope(|s| {
            let handles: Vec<_> = uncached
                .iter()
                .map(|store_path| {
                    let sp = store_path.to_string();
                    s.spawn(move || {
                        let result = nix_store_closure(&sp);
                        (sp, result)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .map_err(|_| anyhow::anyhow!("closure query thread panicked"))
                })
                .collect::<std::result::Result<Vec<_>, _>>()
        })?;

        for (store_path, result) in results {
            match result {
                Ok(closure) => {
                    closure_cache.insert(store_path, closure);
                }
                Err(e) => info!("Warning: closure query failed for {}: {}", store_path, e),
            }
        }
    } else {
        info!("All {} tool closures cached", closure_store_paths.len());
    }
    drop(_closure_span);

    // Build closure vectors from cache
    let rustc_closure = closure_cache.get(&rustc_store).cloned().unwrap_or_default();
    let clippy_closure: Vec<String> = clippy_store
        .as_ref()
        .and_then(|s| closure_cache.get(s).cloned())
        .unwrap_or_default();
    let host_cc_closure = closure_cache
        .get(&host_cc_store)
        .cloned()
        .unwrap_or_default();
    let target_cc_closure = closure_cache
        .get(&target_cc_store)
        .cloned()
        .unwrap_or_default();
    let win_sdk_closure: Vec<String> = win_sdk_store
        .as_ref()
        .and_then(|s| closure_cache.get(s).cloned())
        .unwrap_or_default();

    let mut sys_build_closure: Vec<String> = Vec::new();
    for root in &sys_store_roots {
        if let Some(closure) = closure_cache.get(root) {
            for p in closure {
                if !sys_build_closure.contains(p) {
                    sys_build_closure.push(p.clone());
                }
            }
        }
    }
    if let Some(ref store) = pkg_config_store
        && let Some(closure) = closure_cache.get(store)
    {
        for p in closure {
            if !sys_build_closure.contains(p) {
                sys_build_closure.push(p.clone());
            }
        }
    }

    // Build closure for passthruEnv store paths so that libraries like
    // libclang and their transitive dependencies are available in build
    // script sandboxes.
    let mut passthru_closure: Vec<String> = Vec::new();
    for root in &passthru_store_roots {
        if let Some(closure) = closure_cache.get(root) {
            for p in closure {
                if !passthru_closure.contains(p) {
                    passthru_closure.push(p.clone());
                }
            }
        }
    }

    // Pre-flight: warn about system libraries that pkg-config can't find.
    check_system_libraries(&nix_units, &pkg_config_bin, &pkg_config_path_env);

    // Build key→index map for looking up dep info
    let key_to_idx: HashMap<String, usize> = nix_units
        .iter()
        .enumerate()
        .map(|(i, u)| (u.key.clone(), i))
        .collect();

    let topo_levels = {
        let _s = tracing::info_span!("compute_topo_levels", units = nix_units.len()).entered();
        compute_topo_levels(&nix_units)
    };

    info!(
        "Derivation DAG: {} levels, widest has {} units",
        topo_levels.len(),
        topo_levels.iter().map(|l| l.len()).max().unwrap_or(0)
    );

    // Register derivations level by level via in-process ATerm + daemon socket.
    let mut dep_drv_map: HashMap<String, String> = HashMap::new();
    let mut cache_hits = 0usize;
    let mut cache_misses = 0usize;

    let mut daemon: Option<NixDaemonConn> = if !verify_drv_paths {
        match NixDaemonConn::connect() {
            Ok(conn) => {
                info!("Connected to Nix daemon");
                Some(conn)
            }
            Err(e) => {
                info!(
                    "Cannot connect to Nix daemon ({}), falling back to nix derivation add",
                    e
                );
                None
            }
        }
    } else {
        None // verify mode always uses nix derivation add
    };

    // Cap on how many daemon connections to use in parallel for cache-miss
    // registration. Honoured per level (capped further by the level's
    // miss count), so small levels do not over-allocate. `verify_drv_paths`
    // forces serial execution because the verify path always re-adds via
    // the CLI to compare paths.
    let parallel_jobs = if verify_drv_paths {
        1
    } else {
        let cpu = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        std::cmp::max(1, registration_jobs.unwrap_or(cpu))
    };

    let _register_span = tracing::info_span!(
        "register_derivations",
        levels = topo_levels.len(),
        parallel_jobs = parallel_jobs,
    )
    .entered();
    for (level_idx, level) in topo_levels.iter().enumerate() {
        let _level_span = tracing::info_span!(
            "level",
            idx = level_idx,
            width = level.len(),
        )
        .entered();
        // Construct derivation JSONs — all deps are resolved from previous levels
        let jsons: Vec<(usize, serde_json::Value)> = level
            .iter()
            .map(|&i| {
                // Select host or target linker based on unit classification.
                // system is always host — derivations run on the build machine,
                // rustc's --target flag handles cross-compilation.
                // BuildScriptRun always executes on the host and needs the host
                // cc (even though for_host is false — that flag describes what
                // the output targets, not the execution environment).
                let (unit_cc_bin_dir, unit_cc_closure) =
                    if nix_units[i].for_host || nix_units[i].kind == UnitKind::BuildScriptRun {
                        (host_cc_bin_dir.as_str(), host_cc_closure.as_slice())
                    } else {
                        (target_cc_bin_dir.as_str(), target_cc_closure.as_slice())
                    };
                let json = construct_derivation(
                    &nix_units,
                    i,
                    &key_to_idx,
                    &dep_drv_map,
                    &bash_path,
                    &rustc_str,
                    &rustdoc_str,
                    &proc_macro_rlib,
                    &rustc_store,
                    &mkdir_path,
                    &coreutils_store,
                    unit_cc_bin_dir,
                    unit_cc_closure,
                    &target.nix_system,
                    &rustc_closure,
                    &pkg_config_bin,
                    &pkg_config_path_env,
                    &sys_build_closure,
                    profile,
                    target,
                    &cfg_envs,
                    &host_cfg_envs,
                    &custom_sys_env,
                    passthru_envs,
                    &vendor_dir.to_string_lossy(),
                    &win_sdk_lib_dirs,
                    &win_sdk_closure,
                    &src_str,
                    document_private_items,
                    &passthru_closure,
                    if clippy_str.is_empty() {
                        None
                    } else {
                        Some(&clippy_str)
                    },
                    &clippy_closure,
                    clippy_lint_args,
                    path_prefix_remaps,
                )?;
                tracing::debug!(
                    "Adding derivation for {}: {}",
                    nix_units[i].key,
                    serde_json::to_string_pretty(&json)?
                );
                Ok((i, json))
            })
            .collect::<Result<_>>()?;

        // Pre-compute every unit's `.drv` path, ATerm bytes, and refs so
        // the validity probe can run once for the whole level. Units of
        // a level are dependency-disjoint by construction so all paths
        // can be computed before any daemon round-trip.
        let level_units: Vec<LevelUnit> = jsons
            .into_iter()
            .map(|(i, json)| -> Result<LevelUnit> {
                let drv_file_name = format!("{}.drv", nix_units[i].drv_name);
                let aterm = serialize_derivation_aterm(&json)?;
                let refs = collect_drv_refs(&json);
                let ref_strs: Vec<&str> = refs.iter().map(|s| s.as_str()).collect();
                let drv_path = compute_drv_store_path(&drv_file_name, &aterm, &ref_strs);
                Ok(LevelUnit {
                    i,
                    unit_key: nix_units[i].key.clone(),
                    json,
                    drv_file_name,
                    aterm,
                    refs,
                    drv_path,
                })
            })
            .collect::<Result<_>>()?;

        // Batch the validity probe into a single `wopQueryValidPaths`
        // call. Replaces N per-unit `is_valid_path` round-trips with
        // one. Skipped in `--verify-drv-paths` mode (which always re-
        // adds via the CLI to compare paths) and when the daemon is
        // unavailable (the per-unit path falls through to the CLI).
        let valid_paths: std::collections::HashSet<String> = if verify_drv_paths {
            std::collections::HashSet::new()
        } else if let Some(ref mut conn) = daemon {
            let _s = tracing::info_span!(
                "query_valid_paths_batched",
                n = level_units.len(),
            )
            .entered();
            let paths: Vec<&str> = level_units
                .iter()
                .map(|u| u.drv_path.as_str())
                .collect();
            match conn.query_valid_paths(&paths) {
                Ok(set) => set,
                Err(e) => {
                    info!(
                        "query_valid_paths failed: {}; treating all paths as cache misses",
                        e
                    );
                    std::collections::HashSet::new()
                }
            }
        } else {
            std::collections::HashSet::new()
        };

        // Verify mode short-circuits everything: every unit always re-adds
        // via the CLI so the in-process .drv path can be cross-checked
        // against Nix's. Always serial — `verify_drv_paths` is a debug
        // option, performance does not matter.
        if verify_drv_paths {
            for unit in level_units {
                let nix_path = nix_derivation_add(&unit.json).with_context(|| {
                    format!("Failed to add derivation for {}", unit.unit_key)
                })?;
                if nix_path != unit.drv_path {
                    let drv_content = std::fs::read(&nix_path).unwrap_or_default();
                    let aterm_matches = drv_content == unit.aterm;
                    anyhow::bail!(
                        "Derivation path mismatch for {}:\n  computed: {}\n  nix:      {}\n  aterm matches .drv content: {}",
                        unit.unit_key,
                        unit.drv_path,
                        nix_path,
                        aterm_matches,
                    );
                }
                tracing::debug!("Verified: {} -> {}", unit.unit_key, unit.drv_path);
                cache_misses += 1;
                dep_drv_map.insert(unit.unit_key.clone(), unit.drv_path.clone());
                nix_units[unit.i].drv_path = Some(unit.drv_path);
            }
            continue;
        }

        // Partition the level into cache hits (already in the store) and
        // misses (need registration). The hits go straight to the
        // dep_drv_map; misses are dispatched serially or in parallel
        // depending on `parallel_jobs` and the miss count.
        let (hits, misses): (Vec<LevelUnit>, Vec<LevelUnit>) = level_units
            .into_iter()
            .partition(|u| valid_paths.contains(&u.drv_path));
        cache_hits += hits.len();
        for unit in hits {
            dep_drv_map.insert(unit.unit_key.clone(), unit.drv_path.clone());
            nix_units[unit.i].drv_path = Some(unit.drv_path);
        }

        if misses.is_empty() {
            continue;
        }

        let n_workers = std::cmp::min(parallel_jobs, misses.len());
        cache_misses += misses.len();

        let registered: Vec<(usize, String)> = if n_workers <= 1 {
            // Serial path: reuse the daemon connection across levels.
            let mut out = Vec::with_capacity(misses.len());
            for unit in misses {
                let path = register_unit(&mut daemon, &unit)?;
                info!("Added {} -> {}", unit.unit_key, path);
                out.push((unit.i, path));
            }
            out
        } else {
            // Parallel path: round-robin distribute misses across workers,
            // each spawning a fresh daemon connection inside its scope.
            // Per-worker connections trade ~2 ms × n_workers of handshake
            // for the win of overlapping daemon writes; on the Just bench
            // that is roughly 8 ms vs ~3.6 s of serial registration.
            let _s = tracing::info_span!(
                "parallel_register",
                workers = n_workers,
                misses = misses.len(),
            )
            .entered();
            let chunks = chunk_round_robin(misses, n_workers);
            std::thread::scope(|s| -> Result<Vec<(usize, String)>> {
                let handles: Vec<_> = chunks
                    .into_iter()
                    .map(|chunk| {
                        s.spawn(move || -> Result<Vec<(usize, String)>> {
                            let mut conn = NixDaemonConn::connect().ok();
                            let mut out = Vec::with_capacity(chunk.len());
                            for unit in chunk {
                                let path = register_unit(&mut conn, &unit)?;
                                out.push((unit.i, path));
                            }
                            Ok(out)
                        })
                    })
                    .collect();
                let mut all = Vec::new();
                for h in handles {
                    let chunk_results = h
                        .join()
                        .map_err(|_| anyhow::anyhow!("registration worker panicked"))??;
                    all.extend(chunk_results);
                }
                Ok(all)
            })?
        };

        for (i, drv_path) in registered {
            let key = nix_units[i].key.clone();
            dep_drv_map.insert(key, drv_path.clone());
            nix_units[i].drv_path = Some(drv_path);
        }
    }
    drop(_register_span);

    info!(
        "Derivation registration: {} cached (path exists), {} added",
        cache_hits, cache_misses
    );

    // Collect root derivation paths (all units marked is_root)
    let root_drvs: Vec<(String, String, UnitKind)> = nix_units
        .iter()
        .filter(|u| u.is_root)
        .filter_map(|u| {
            u.drv_path
                .clone()
                .map(|p| (p, u.target_name.clone(), u.kind.clone()))
        })
        .collect();
    if root_drvs.is_empty() {
        // Fallback: use last unit (backward compat for single-package projects)
        let last_drv = nix_units
            .last()
            .and_then(|u| u.drv_path.clone())
            .ok_or_else(|| anyhow::anyhow!("No units to build"))?;
        let last_name = nix_units
            .last()
            .map(|u| u.crate_name.clone())
            .unwrap_or_default();
        return Ok((
            vec![(last_drv, last_name, UnitKind::Compile)],
            nix_units,
            cfg_envs,
            host_cfg_envs,
        ));
    }

    Ok((root_drvs, nix_units, cfg_envs, host_cfg_envs))
}

/// Build and register an aggregator derivation that depends on every
/// element of `root_drvs` and produces an output containing one
/// symlink per root pointing at that root's `out`.
///
/// The aggregator gives downstream Nix expressions a *single*
/// derivation reference to `builtins.outputOf`, sidestepping the
/// realisation conflict that hits per-root wrapper derivations
/// whenever any plan-listed root is transitively depended on by
/// another (every `cargo build/check/clippy --workspace` invocation
/// in a multi-crate workspace).  The aggregator's transitive deps
/// realise each root drv exactly once at its natural store path.
///
/// Returns the registered aggregator's `.drv` store path.
pub(crate) fn construct_aggregator_drv(
    pname: &str,
    intent: &str,
    root_drvs: &[(String, String, UnitKind)],
    system: &str,
) -> Result<String> {
    let bash_path = which_command("bash")?.to_string_lossy().to_string();
    let bash_store = std::path::PathBuf::from(&bash_path)
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Cannot derive bash store path"))?
        .to_string_lossy()
        .to_string();
    let mkdir_path = which_command_no_deref("mkdir")?
        .to_string_lossy()
        .to_string();
    let coreutils_store = std::path::PathBuf::from(&mkdir_path)
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| anyhow::anyhow!("Cannot derive coreutils store path from mkdir"))?
        .to_string_lossy()
        .to_string();

    let mut input_drvs = serde_json::Map::new();
    let mut placeholders: Vec<String> = Vec::with_capacity(root_drvs.len());
    for (drv_path, _, _) in root_drvs {
        placeholders.push(downstream_placeholder(drv_path, "out")?);
        input_drvs.insert(
            drv_path.clone(),
            serde_json::json!({"outputs": ["out"], "dynamicOutputs": {}}),
        );
    }

    // Aggregator build script: each root drv's realised output path
    // arrives via the `rootOuts` env var (space-separated).  Symlink
    // each into `$out/root-N`.  Symlink rather than copy keeps the
    // aggregator's NAR small and avoids file-mode quirks; consumers
    // walk the symlinks transparently.
    let script = format!(
        "set -e\n\
         {coreutils}/bin/mkdir -p $out\n\
         i=0\n\
         for src in $rootOuts; do\n\
           {coreutils}/bin/ln -s \"$src\" \"$out/root-$i\"\n\
           i=$((i+1))\n\
         done\n",
        coreutils = coreutils_store
    );

    let mut env = serde_json::Map::new();
    env.insert(
        "out".into(),
        serde_json::Value::String(self_placeholder("out")),
    );
    env.insert(
        "rootOuts".into(),
        serde_json::Value::String(placeholders.join(" ")),
    );

    let json = serde_json::json!({
        "name": format!("{}-{}-aggregator", pname, intent),
        "system": system,
        "builder": bash_path,
        "args": ["-c", script],
        "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}},
        "inputDrvs": input_drvs,
        "inputSrcs": [coreutils_store, bash_store],
        "env": env,
    });

    nix_derivation_add(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_round_robin_distributes_evenly() {
        let chunks = chunk_round_robin(vec![0, 1, 2, 3, 4, 5, 6, 7], 4);
        assert_eq!(chunks, vec![vec![0, 4], vec![1, 5], vec![2, 6], vec![3, 7]]);
    }

    #[test]
    fn chunk_round_robin_handles_uneven_input() {
        // 5 items into 3 chunks: round-robin gives 2/2/1.
        let chunks = chunk_round_robin(vec!['a', 'b', 'c', 'd', 'e'], 3);
        assert_eq!(chunks, vec![vec!['a', 'd'], vec!['b', 'e'], vec!['c']]);
    }

    #[test]
    fn chunk_round_robin_handles_overprovisioned_workers() {
        // More chunks than items: trailing chunks are empty.
        let chunks = chunk_round_robin(vec![1, 2], 4);
        assert_eq!(chunks, vec![vec![1], vec![2], vec![], vec![]]);
    }

    #[test]
    fn chunk_round_robin_handles_empty_input() {
        let chunks: Vec<Vec<i32>> = chunk_round_robin(vec![], 3);
        assert_eq!(chunks, vec![Vec::<i32>::new(); 3]);
    }
}
