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
mod unit_graph;
pub(crate) mod util;

use aterm::{collect_drv_refs, compute_drv_store_path, serialize_derivation_aterm};
use daemon::NixDaemonConn;
use derivation::{construct_derivation, nix_derivation_add, nix_store_closure};
use unit_graph::{compute_topo_levels, extract_units_from_bcx};
use util::{
    find_cross_linker, find_sysroot_rlib, which_command, which_command_no_deref, which_rustc,
};

use anyhow::{Context, Result};
use cargo::core::Workspace;
use cargo::core::compiler::UnitInterner;
use cargo::ops::{self, CompileOptions};
use cargo::util::command_prelude::UserIntent;
use cargo::util::context::GlobalContext;
use log::info;
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
            log::warn!(
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
    features: &[String],
    no_default_features: bool,
    passthru_envs: &[(String, String)],
    project_dir: Option<&Path>,
) -> Result<(
    Vec<(String, String)>,
    Vec<NixUnit>,
    Vec<(String, String)>,
    Vec<(String, String)>,
)> {
    let manifest_path = src.join("Cargo.toml");
    if !manifest_path.exists() {
        anyhow::bail!("No Cargo.toml found at {}", manifest_path.display());
    }

    // Read custom sys-env overrides from [workspace.metadata.schnee.sys-env]
    // or [package.metadata.schnee.sys-env] in the root Cargo.toml.
    let custom_sys_env = read_custom_sys_env(&manifest_path);

    let src_str = src.to_string_lossy().to_string();

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
        // Package selection: -p/--package narrows the build to specific crates
        if !packages.is_empty() {
            options.spec = ops::Packages::Packages(packages.to_vec());
        } else if ws.is_virtual() {
            // For virtual workspaces (no [package] in root), build all members
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

        // Extract unit graph and target cfg — NO compilation happens here
        let interner = UnitInterner::new();
        let bcx = ops::create_bcx(&ws, &options, &interner, None)?;
        let units = extract_units_from_bcx(&bcx, &bcx.roots, src, vendor_dir)?;
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

        (units, bcx_cfg_envs, bcx_host_cfg_envs)
    };

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

    // Deduplicate
    closure_store_paths.sort();
    closure_store_paths.dedup();

    // Separate cached vs uncached store paths
    let uncached: Vec<&str> = closure_store_paths
        .iter()
        .filter(|p| !closure_cache.contains_key(p.as_str()))
        .map(|p| p.as_str())
        .collect();

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

    // Build closure vectors from cache
    let rustc_closure = closure_cache.get(&rustc_store).cloned().unwrap_or_default();
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

    // Pre-flight: warn about system libraries that pkg-config can't find.
    check_system_libraries(&nix_units, &pkg_config_bin, &pkg_config_path_env);

    // Build key→index map for looking up dep info
    let key_to_idx: HashMap<String, usize> = nix_units
        .iter()
        .enumerate()
        .map(|(i, u)| (u.key.clone(), i))
        .collect();

    let topo_levels = compute_topo_levels(&nix_units);

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

    for level in &topo_levels {
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
                )?;
                log::debug!(
                    "Adding derivation for {}: {}",
                    nix_units[i].key,
                    serde_json::to_string_pretty(&json)?
                );
                Ok((i, json))
            })
            .collect::<Result<_>>()?;

        for (i, json) in jsons {
            let drv_file_name = format!("{}.drv", nix_units[i].drv_name);
            let aterm = serialize_derivation_aterm(&json)?;
            let refs = collect_drv_refs(&json);
            let ref_strs: Vec<&str> = refs.iter().map(|s| s.as_str()).collect();
            let drv_path = compute_drv_store_path(&drv_file_name, &aterm, &ref_strs);

            if verify_drv_paths {
                let nix_path = nix_derivation_add(&json).with_context(|| {
                    format!("Failed to add derivation for {}", nix_units[i].key)
                })?;
                if nix_path != drv_path {
                    let drv_content = std::fs::read(&nix_path).unwrap_or_default();
                    let aterm_matches = drv_content == aterm;
                    anyhow::bail!(
                        "Derivation path mismatch for {}:\n  computed: {}\n  nix:      {}\n  aterm matches .drv content: {}",
                        nix_units[i].key,
                        drv_path,
                        nix_path,
                        aterm_matches,
                    );
                }
                log::debug!("Verified: {} -> {}", nix_units[i].key, drv_path);
            } else if daemon
                .as_mut()
                .and_then(|c| c.is_valid_path(&drv_path).ok())
                .unwrap_or(false)
            {
                cache_hits += 1;
            } else if let Some(ref mut conn) = daemon {
                match conn.add_text_to_store(&drv_file_name, &aterm, &ref_strs) {
                    Ok(result_path) => {
                        if result_path != drv_path {
                            anyhow::bail!(
                                "Daemon path mismatch for {}:\n  computed: {}\n  daemon:   {}",
                                nix_units[i].key,
                                drv_path,
                                result_path,
                            );
                        }
                        cache_misses += 1;
                        info!("Added {} -> {}", nix_units[i].key, drv_path);
                    }
                    Err(e) => {
                        info!(
                            "Daemon error for {}: {}, attempting reconnect",
                            nix_units[i].key, e
                        );
                        match NixDaemonConn::connect() {
                            Ok(mut new_conn) => {
                                match new_conn.add_text_to_store(&drv_file_name, &aterm, &ref_strs)
                                {
                                    Ok(result_path) => {
                                        if result_path != drv_path {
                                            anyhow::bail!(
                                                "Daemon path mismatch for {}:\n  computed: {}\n  daemon:   {}",
                                                nix_units[i].key,
                                                drv_path,
                                                result_path,
                                            );
                                        }
                                        daemon = Some(new_conn);
                                        cache_misses += 1;
                                        info!(
                                            "Reconnected and added {} -> {}",
                                            nix_units[i].key, drv_path
                                        );
                                    }
                                    Err(e2) => {
                                        info!(
                                            "Reconnected daemon also failed for {}: {}, falling back to process",
                                            nix_units[i].key, e2
                                        );
                                        daemon = None;
                                        nix_derivation_add(&json).with_context(|| {
                                            format!(
                                                "Failed to add derivation for {}",
                                                nix_units[i].key
                                            )
                                        })?;
                                        cache_misses += 1;
                                    }
                                }
                            }
                            Err(e2) => {
                                info!("Daemon reconnect failed ({}), falling back to process", e2);
                                daemon = None;
                                nix_derivation_add(&json).with_context(|| {
                                    format!("Failed to add derivation for {}", nix_units[i].key)
                                })?;
                                cache_misses += 1;
                            }
                        }
                    }
                }
            } else {
                // Fallback: spawn nix derivation add process
                nix_derivation_add(&json).with_context(|| {
                    format!("Failed to add derivation for {}", nix_units[i].key)
                })?;
                cache_misses += 1;
            }

            dep_drv_map.insert(nix_units[i].key.clone(), drv_path.clone());
            nix_units[i].drv_path = Some(drv_path);
        }
    }

    info!(
        "Derivation registration: {} cached (path exists), {} added",
        cache_hits, cache_misses
    );

    // Collect root derivation paths (all units marked is_root)
    let root_drvs: Vec<(String, String)> = nix_units
        .iter()
        .filter(|u| u.is_root)
        .filter_map(|u| u.drv_path.clone().map(|p| (p, u.target_name.clone())))
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
            vec![(last_drv, last_name)],
            nix_units,
            cfg_envs,
            host_cfg_envs,
        ));
    }

    Ok((root_drvs, nix_units, cfg_envs, host_cfg_envs))
}
