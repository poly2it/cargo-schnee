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
    find_cross_linker, find_sysroot_rlib, which_bash, which_clippy_driver, which_command_no_deref,
    which_rustc, which_rustdoc,
};

use anyhow::{Context, Result};
use cargo::core::Workspace;
use cargo::core::compiler::UnitInterner;
use cargo::ops::{self, CompileOptions};
use cargo::util::command_prelude::UserIntent;
use cargo::util::context::GlobalContext;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::info;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

/// A caller-supplied unit setup rule, deserialised from the JSON in
/// `CARGO_SCHNEE_UNIT_SETUP` (see `nix/buildPackage.nix`'s `unitSetup`).
/// Every rule matching a unit contributes its `script`, sourced inside
/// the unit's sandbox — in list order — immediately before the compiler,
/// rustdoc, clippy-driver, or build-script invocation runs.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitSetupRule {
    /// Cargo package name, or "*" to match every package. Matches
    /// vendored dependencies as well as workspace members.
    pub package: String,
    /// Unit kinds the rule applies to, kebab-case. Defaults to the four
    /// macro-expansion kinds — compile, check, test-compile, doc; the
    /// build-script kinds must be named explicitly.
    #[serde(
        default = "default_unit_setup_kinds",
        deserialize_with = "de_unit_setup_kinds"
    )]
    pub kinds: Vec<UnitKind>,
    /// Optional filter on cargo target names. `None` matches all targets.
    #[serde(default)]
    pub targets: Option<Vec<String>>,
    /// Store path of the shell script sourced into matching units.
    pub script: String,
}

fn default_unit_setup_kinds() -> Vec<UnitKind> {
    vec![
        UnitKind::Compile,
        UnitKind::Check,
        UnitKind::TestCompile,
        UnitKind::Doc,
    ]
}

fn de_unit_setup_kinds<'de, D>(d: D) -> std::result::Result<Vec<UnitKind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    Vec::<String>::deserialize(d)?
        .iter()
        .map(|s| match s.as_str() {
            "compile" => Ok(UnitKind::Compile),
            "check" => Ok(UnitKind::Check),
            "doc" => Ok(UnitKind::Doc),
            "test-compile" => Ok(UnitKind::TestCompile),
            "build-script-compile" => Ok(UnitKind::BuildScriptCompile),
            "build-script-run" => Ok(UnitKind::BuildScriptRun),
            other => Err(serde::de::Error::custom(format!(
                "unknown unit kind {other:?}; expected one of compile, check, \
                 test-compile, doc, build-script-compile, build-script-run"
            ))),
        })
        .collect()
}

/// Whether `rule` matches `unit`: its `package` equals the unit's cargo
/// package name (or is "*"), its `kinds` contain the unit's kind, and its
/// `targets` filter — when present — names the unit's cargo target.
/// Target names are compared with `-` collapsed to `_`, mirroring how the
/// unit's `crate_name` derives from `unit.target.name()`; clippy needs no
/// special casing because clippy units are Check units with a swapped
/// driver, so kind matching covers them by construction.
fn unit_setup_rule_matches(rule: &UnitSetupRule, unit: &NixUnit) -> bool {
    let pkg_name = unit
        .cargo_envs
        .iter()
        .find(|(k, _)| k == "CARGO_PKG_NAME")
        .map(|(_, v)| v.as_str());
    (rule.package == "*" || pkg_name == Some(rule.package.as_str()))
        && rule.kinds.contains(&unit.kind)
        && rule.targets.as_ref().is_none_or(|targets| {
            targets
                .iter()
                .any(|t| t.replace('-', "_") == unit.crate_name)
        })
}

/// Collect the setup scripts matching `unit`, in rule order.
pub(crate) fn unit_setup_scripts(unit: &NixUnit, rules: &[UnitSetupRule]) -> Vec<String> {
    rules
        .iter()
        .filter(|rule| unit_setup_rule_matches(rule, unit))
        .map(|rule| rule.script.clone())
        .collect()
}

/// Indices of rules matching no unit in the plan. A no-match rule is
/// usually a typo'd package or target name and would otherwise no-op
/// silently, so callers surface each returned index as a warning. It is
/// only a warning because the same rule list is legitimately shared
/// between build, test, and clippy packages, and e.g. a `test-compile`
/// rule matches nothing in a plain build plan.
pub(crate) fn unmatched_unit_setup_rules(units: &[NixUnit], rules: &[UnitSetupRule]) -> Vec<usize> {
    rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| !units.iter().any(|unit| unit_setup_rule_matches(rule, unit)))
        .map(|(i, _)| i)
        .collect()
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
    /// Whether this unit must be compiled with `--test`.  Set for any
    /// integration/unit/example/bench target included via cargo's
    /// `--all-targets` selection: cargo's bcx represents these as
    /// `CompileMode::Check { test: true }` (or `CompileMode::Test`),
    /// and rustc needs `--test` to synthesise a `main` and the test
    /// harness.  Without it, integration test crates fail at compile
    /// time with E0601 (`main function not found in crate ...`).
    #[serde(default)]
    pub(crate) compile_test: bool,
    /// Whether this local crate's build script is self-contained — it reads
    /// only its own crate directory and inputs provided via the environment,
    /// never sibling directories like `../spec/`. Opt in via
    /// `[package.metadata.schnee] self-contained-build-script = true`. When set,
    /// the BuildScriptRun unit is sliced to its own per-crate source (Change 3)
    /// instead of carrying the whole workspace tree, so it stops re-keying on
    /// unrelated edits.
    #[serde(default)]
    pub(crate) self_contained_build_script: bool,
    /// Set when this local unit was per-crate sliced off the project-src tree
    /// (see `assign_per_crate_src_stores`): the crate's directory relative to
    /// the project-src root, e.g. `skeptiva-ai-common`. Slicing rewrites the
    /// unit's source onto a flat `<hash>-<member>` store, which severs the
    /// crate's position in the workspace; `--remap-path-prefix` rules that are
    /// expressed relative to the project-src root must re-append this so
    /// diagnostics show `<replacement>/<crate_rel>/...` rather than collapsing
    /// to `<replacement>/...` and dropping the member directory. `None` for
    /// non-sliced units and vendored crates.
    #[serde(default)]
    pub(crate) sliced_crate_rel: Option<String>,
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

// ---------------------------------------------------------------------------
// Declared feature-resolution scope
// ---------------------------------------------------------------------------

/// The package selection cargo resolves features over, independent of which
/// packages the caller actually asked to build.
///
/// Cargo unifies features across whatever a single command line names, so
/// `-p a` and `-p b` produce two different graphs even where they overlap.
/// A declared scope pins the resolution input for every invocation that
/// names it, and `-p` is demoted to a post-resolution root filter — so
/// sibling builds share their unit derivations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionSpec {
    /// Every workspace member, mirroring `cargo --workspace`.
    Workspace,
    /// A hand-picked member list, mirroring `cargo -p`. `exclude` has
    /// already been subtracted.
    Packages(Vec<String>),
    /// Every workspace member bar the listed ones, mirroring
    /// `cargo --workspace --exclude`.
    Exclude(Vec<String>),
}

impl ResolutionSpec {
    /// Stable textual form of the resolved selection. Feeds the unit-graph
    /// cache key so two invocations that name the same scope look up the
    /// same entry.
    fn cache_component(&self) -> String {
        match self {
            Self::Workspace => "workspace".to_string(),
            Self::Packages(p) => format!("packages={}", p.join(",")),
            Self::Exclude(e) => format!("exclude={}", e.join(",")),
        }
    }
}

/// A resolution scope together with the manifest key it was read from. The
/// key is carried so diagnostics can name the exact table entry the user
/// has to edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionScope {
    pub key: String,
    pub spec: ResolutionSpec,
}

impl ResolutionScope {
    /// `workspace.metadata.schnee.resolution.<key>` — the manifest path
    /// every scope diagnostic points at.
    pub fn manifest_key(&self) -> String {
        format!("workspace.metadata.schnee.resolution.{}", self.key)
    }

    /// Cache-key component: the requested key plus the selection it
    /// resolved to.
    pub fn cache_component(&self) -> String {
        format!("scope:{}:{}", self.key, self.spec.cache_component())
    }
}

/// Read a declared resolution scope from
/// `[workspace.metadata.schnee.resolution]` in the workspace manifest.
///
/// The table is keyed by target triple with a `default` fallback; `key` is
/// looked up first and `default` second. A value is either the string
/// `"workspace"` or a table with `packages` and an optional `exclude`.
///
/// Read only from the `workspace` table, never `package`: a member
/// declaring its own scope would reintroduce exactly the per-invocation
/// divergence the scope exists to remove.
pub fn read_resolution_scope(manifest_path: &Path, key: &str) -> Result<ResolutionScope> {
    let content = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    let doc: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    let table = doc
        .get("workspace")
        .and_then(|w| w.get("metadata"))
        .and_then(|m| m.get("schnee"))
        .and_then(|s| s.get("resolution"))
        .and_then(|v| v.as_table())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "resolution scope {:?} was requested but {} declares no \
                 [workspace.metadata.schnee.resolution] table",
                key,
                manifest_path.display(),
            )
        })?;
    let (resolved_key, value) = match table.get(key) {
        Some(v) => (key.to_string(), v),
        None => match table.get("default") {
            Some(v) => ("default".to_string(), v),
            None => anyhow::bail!(
                "workspace.metadata.schnee.resolution in {} has neither a {:?} \
                 key nor a `default` key",
                manifest_path.display(),
                key,
            ),
        },
    };
    let spec = parse_resolution_spec(value, &resolved_key, manifest_path)?;
    Ok(ResolutionScope {
        key: resolved_key,
        spec,
    })
}

fn parse_resolution_spec(
    value: &toml::Value,
    key: &str,
    manifest_path: &Path,
) -> Result<ResolutionSpec> {
    let where_ = format!(
        "workspace.metadata.schnee.resolution.{} in {}",
        key,
        manifest_path.display(),
    );
    if let Some(s) = value.as_str() {
        anyhow::ensure!(
            s == "workspace",
            "{}: the only accepted string value is \"workspace\" (got {:?})",
            where_,
            s,
        );
        return Ok(ResolutionSpec::Workspace);
    }
    let table = value
        .as_table()
        .ok_or_else(|| anyhow::anyhow!("{}: expected \"workspace\" or a table", where_))?;
    for name in table.keys() {
        anyhow::ensure!(
            name == "packages" || name == "exclude",
            "{}: unknown field {:?}, expected `packages` or `exclude`",
            where_,
            name,
        );
    }
    let string_list = |field: &str| -> Result<Vec<String>> {
        match table.get(field) {
            None => Ok(Vec::new()),
            Some(v) => v
                .as_array()
                .ok_or_else(|| {
                    anyhow::anyhow!("{}: `{}` must be an array of strings", where_, field)
                })?
                .iter()
                .map(|e| {
                    e.as_str().map(String::from).ok_or_else(|| {
                        anyhow::anyhow!("{}: `{}` must be an array of strings", where_, field)
                    })
                })
                .collect(),
        }
    };
    let packages = string_list("packages")?;
    let exclude = string_list("exclude")?;
    if packages.is_empty() {
        anyhow::ensure!(
            !exclude.is_empty(),
            "{}: needs `packages`, `exclude`, or the string \"workspace\"",
            where_,
        );
        return Ok(ResolutionSpec::Exclude(exclude));
    }
    // `exclude` subtracts from an explicit `packages` list. Cargo itself
    // only accepts `--exclude` alongside `--workspace`, so the subtraction
    // happens here rather than being handed to the resolver.
    let kept: Vec<String> = packages
        .into_iter()
        .filter(|p| !exclude.contains(p))
        .collect();
    anyhow::ensure!(
        !kept.is_empty(),
        "{}: `exclude` removes every entry of `packages`",
        where_,
    );
    Ok(ResolutionSpec::Packages(kept))
}

/// The cargo package name a unit belongs to, taken from the standard
/// `CARGO_PKG_NAME` env every unit carries.
fn unit_package_name(unit: &NixUnit) -> &str {
    unit.cargo_envs
        .iter()
        .find(|(k, _)| k == "CARGO_PKG_NAME")
        .map(|(_, v)| v.as_str())
        .unwrap_or("")
}

/// Narrow a scope-wide unit graph to the packages the caller asked for.
///
/// `units` is the full graph cargo resolved over the declared scope, with
/// `is_root` set on every scope root. This picks the roots belonging to
/// `packages` (minus `exclude`), clears `is_root` elsewhere, and drops
/// every unit no longer reachable from the surviving roots.
///
/// Because the resolution input is the scope rather than the request, two
/// invocations naming different packages within one scope produce
/// byte-identical derivations for the units they share — which is the
/// whole point of declaring a scope.
///
/// An empty `packages` means "every root in the scope", so the graph is
/// returned untouched.
pub fn narrow_to_requested_roots(
    units: Vec<NixUnit>,
    packages: &[String],
    exclude: &[String],
    scope: &ResolutionScope,
) -> Result<Vec<NixUnit>> {
    let requested: Vec<&String> = packages.iter().filter(|p| !exclude.contains(p)).collect();
    if packages.is_empty() && exclude.is_empty() {
        return Ok(units);
    }

    // `packages` empty and `requested` empty mean different things, and
    // conflating them inverts the selection: `-p a --exclude a` would fall
    // into the scope-wide arm below and build every root except `a`, which
    // is the opposite of what the caller asked for.
    anyhow::ensure!(
        !(requested.is_empty() && !packages.is_empty()),
        "every requested package is also excluded: -p {} with --exclude {} \
         selects nothing to build",
        packages.join(", "),
        exclude.join(", "),
    );

    // Hard error rather than a silent empty build: a package outside the
    // scope means the manifest and the build definition disagree, and the
    // manifest key is the thing that has to change.
    let scope_packages: HashSet<&str> = units
        .iter()
        .filter(|u| u.is_root)
        .map(unit_package_name)
        .collect();
    for pkg in &requested {
        anyhow::ensure!(
            scope_packages.contains(pkg.as_str()),
            "package `{}` is not in the resolution scope declared at {}. \
             Scope roots: {}",
            pkg,
            scope.manifest_key(),
            {
                let mut names: Vec<&str> = scope_packages.iter().copied().collect();
                names.sort_unstable();
                names.join(", ")
            },
        );
    }

    let mut units = units;
    let mut root_indices: Vec<usize> = Vec::new();
    for (i, unit) in units.iter_mut().enumerate() {
        if !unit.is_root {
            continue;
        }
        let pkg = unit_package_name(unit);
        let keep = if packages.is_empty() {
            !exclude.iter().any(|e| e == pkg)
        } else {
            requested.iter().any(|p| p.as_str() == pkg)
        };
        if keep {
            root_indices.push(i);
        } else {
            unit.is_root = false;
        }
    }

    let key_to_idx: HashMap<&str, usize> = units
        .iter()
        .enumerate()
        .map(|(i, u)| (u.key.as_str(), i))
        .collect();

    // Reachability walk. `all_dep_keys` skips BuildScriptRun units and
    // `links_dep_keys` is populated only on them, so neither is a subset
    // of the other and both have to be walked. Dropping a links dep would
    // silently strip a build script's `DEP_*` environment, so a dangling
    // one is an error, not a skip.
    let mut reachable: HashSet<usize> = HashSet::new();
    let mut stack: Vec<usize> = root_indices.clone();
    while let Some(idx) = stack.pop() {
        if !reachable.insert(idx) {
            continue;
        }
        let unit = &units[idx];
        let direct = unit.dep_extern.iter().map(|(_, k)| k.as_str());
        let transitive = unit.all_dep_keys.iter().map(|k| k.as_str());
        let scripts = unit
            .build_script_dep
            .iter()
            .chain(unit.build_script_compile_key.iter())
            .map(|k| k.as_str());
        for dep_key in direct.chain(transitive).chain(scripts) {
            if let Some(&dep_idx) = key_to_idx.get(dep_key) {
                stack.push(dep_idx);
            }
        }
        for (dep_key, links_name) in &unit.links_dep_keys {
            let dep_idx = key_to_idx.get(dep_key.as_str()).copied().ok_or_else(|| {
                anyhow::anyhow!(
                    "unit {} declares a links dependency on `{}` (key {}) that \
                     is not in the plan; pruning it would strip the build \
                     script's DEP_* environment",
                    unit.key,
                    links_name,
                    dep_key,
                )
            })?;
            stack.push(dep_idx);
        }
    }

    let before = units.len();
    drop(key_to_idx);
    let kept: Vec<NixUnit> = units
        .into_iter()
        .enumerate()
        .filter(|(i, _)| reachable.contains(i))
        .map(|(_, u)| u)
        .collect();
    tracing::info!(
        scope = %scope.manifest_key(),
        roots = root_indices.len(),
        kept = kept.len(),
        pruned = before - kept.len(),
        "Narrowed scope graph to requested roots",
    );
    Ok(kept)
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

/// Source-replacement entries that redirect every git dependency found in
/// `lock_text` (a `Cargo.lock`) to the vendored copy, mirroring what
/// `cargo vendor` emits. Without these the offline planner has only the
/// crates.io redirect, so cargo falls back to a (sandbox-forbidden) network
/// fetch for any git dependency.
fn git_source_overrides(lock_text: &str) -> String {
    use std::collections::BTreeSet;
    let mut seen = BTreeSet::new();
    let mut out = String::new();
    for line in lock_text.lines() {
        let Some(rest) = line.trim().strip_prefix("source = \"git+") else {
            continue;
        };
        let Some(src) = rest.strip_suffix('"') else {
            continue;
        };
        // The replacement key is the source id without the resolved `#<commit>`.
        let no_frag = src.split('#').next().unwrap_or(src);
        let key = format!("git+{no_frag}");
        if !seen.insert(key.clone()) {
            continue;
        }
        let (url, query) = match no_frag.split_once('?') {
            Some((u, q)) => (u, Some(q)),
            None => (no_frag, None),
        };
        out.push_str(&format!("\n[source.\"{key}\"]\ngit = \"{url}\"\n"));
        // Carry a `?rev=` / `?branch=` / `?tag=` ref over verbatim.
        if let Some(q) = query {
            for kv in q.split('&') {
                if let Some((k, v)) = kv.split_once('=') {
                    if matches!(k, "rev" | "branch" | "tag") {
                        out.push_str(&format!("{k} = \"{v}\"\n"));
                    }
                }
            }
        }
        out.push_str("replace-with = \"vendored-sources\"\n");
    }
    out
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
    // Declared feature-resolution scope. When present it — not the `-p`
    // list — is what cargo resolves over, so every invocation naming the
    // same scope gets the same graph; `packages` then only picks roots
    // out of it (see `narrow_to_requested_roots`).
    resolution: Option<&ResolutionScope>,
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
    let mut cargo_config = format!(
        "[source.crates-io]\nreplace-with = \"vendored-sources\"\n\n[source.vendored-sources]\ndirectory = \"{}\"\n",
        vendor_dir.display()
    );
    // crates.io is redirected above; git dependencies each need their own
    // `[source."git+…"]` redirect, derived from the lockfile, or cargo tries to
    // fetch them from the network.
    if let Ok(lock_text) = std::fs::read_to_string(src.join("Cargo.lock")) {
        cargo_config.push_str(&git_source_overrides(&lock_text));
    }
    std::fs::write(cargo_home.join("config.toml"), cargo_config)?;
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
        options.build_config.requested_kinds = vec![cargo::core::compiler::CompileKind::Target(
            cargo::core::compiler::CompileTarget::new(&target.target_triple)?,
        )];
    }
    // Package selection.  With a declared scope the selection comes from
    // the manifest and is identical for every invocation naming that
    // scope, which is what lets their unit derivations be shared; -p and
    // --exclude are applied afterwards, to the roots only.  Without one,
    // -p/--package narrows the build to specific crates and --exclude
    // removes crates from the default workspace set.
    if let Some(scope) = resolution {
        tracing::info!(
            scope = %scope.manifest_key(),
            "Resolving features over declared scope",
        );
        options.spec = match &scope.spec {
            ResolutionSpec::Workspace => ops::Packages::All(Vec::new()),
            ResolutionSpec::Packages(p) => ops::Packages::Packages(p.clone()),
            ResolutionSpec::Exclude(e) => ops::Packages::OptOut(e.clone()),
        };
    } else if !packages.is_empty() {
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
    let bcx_cfg_envs = extract_cfg_envs(bcx.target_data.cfg(bcx.build_config.requested_kinds[0]));
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
/// Assign each unit its source store path. Local compile/test/doc units are
/// sliced to a per-crate NAR — decoupled from sibling crates — via `add`, which
/// adds a crate subtree to the store and returns its content-addressed path;
/// the unit's `source_file`/`manifest_dir` are rewritten onto that store.
/// BuildScriptRun units keep the whole-tree `src_str`, because they copy the
/// workspace into their workdir so sibling reads like `../spec/` resolve (see
/// `build_run_script`); slicing them needs the spec-via-env change first. `add`
/// is injected so the slicing logic is testable without a Nix daemon.
fn assign_per_crate_src_stores(
    units: &mut [NixUnit],
    src_str: &str,
    vendor_str: &str,
    mut add: impl FnMut(&str) -> Result<String>,
) -> Result<Vec<String>> {
    let mut unit_src_store: Vec<String> = vec![src_str.to_string(); units.len()];
    let mut per_crate: HashMap<String, String> = HashMap::new();
    for i in 0..units.len() {
        // Pick the root this unit's source lives under and whether to slice it.
        //
        // Change 1 (local crates): slice compile/test/doc units. A local
        // BuildScriptRun normally copies the whole workspace into its workdir so
        // sibling reads like `../spec/` resolve (see build_run_script), so it is
        // NOT sliced — UNLESS the crate opts in via
        // `[package.metadata.schnee] self-contained-build-script` (Change 3),
        // declaring its build script reads only its own dir and env inputs.
        //
        // Change 2 (vendored deps): slice every unit off the aggregate vendor
        // dir. A vendored crate is self-contained — its build script reads only
        // its own files — so per-crate slicing is always safe, and it decouples
        // the lock axis: a `Cargo.lock` bump that changes one dep no longer
        // re-keys the units of unrelated deps (which kept the same per-crate
        // store) the way the shared aggregate did.
        let (is_local, is_bsr, self_contained_bs) = {
            let u = &units[i];
            (
                u.is_local,
                matches!(u.kind, UnitKind::BuildScriptRun),
                u.self_contained_build_script,
            )
        };
        let (root, sliceable) = if is_local {
            (src_str, !is_bsr || self_contained_bs)
        } else {
            (vendor_str, !vendor_str.is_empty())
        };
        if !sliceable {
            continue;
        }
        let (crate_rel, source_file, manifest_dir) = {
            let u = &units[i];
            let crate_rel = u
                .manifest_dir
                .strip_prefix(root)
                .map(|s| s.trim_start_matches('/').to_string())
                .filter(|s| !s.is_empty());
            (crate_rel, u.source_file.clone(), u.manifest_dir.clone())
        };
        let Some(crate_rel) = crate_rel else { continue };
        // Key the cache on (root, crate_rel) so a local and a vendored crate
        // sharing a name never collide.
        let cache_key = format!("{root}\u{0}{crate_rel}");
        let crate_store = match per_crate.get(&cache_key) {
            Some(p) => p.clone(),
            None => {
                let full = format!("{}/{}", root, crate_rel);
                // Local crates: add the subtree to the store. Vendored crates
                // are already content-addressed per-crate store paths behind the
                // cargo-vendor-dir symlink farm, so follow the symlink to its
                // target rather than re-adding — re-adding would capture the
                // symlink itself, whose target is not mounted in the compile
                // sandbox. If the vendor entry is a real copy rather than a
                // symlink, canonicalize returns it unchanged (correct, but not
                // decoupled).
                let p = if is_local {
                    add(&full)?
                } else {
                    std::fs::canonicalize(&full)
                        .with_context(|| format!("resolving vendored crate at {full}"))?
                        .to_string_lossy()
                        .to_string()
                };
                per_crate.insert(cache_key, p.clone());
                p
            }
        };
        // Rewrite the unit's source paths from `{root}/{crate_rel}` onto the
        // crate-rooted store, and reference it as this unit's src_store.
        let old_prefix = format!("{}/{}", root, crate_rel);
        let u = &mut units[i];
        if let Some(rest) = source_file.strip_prefix(&old_prefix) {
            u.source_file = format!("{}{}", crate_store, rest);
        }
        if manifest_dir == old_prefix {
            u.manifest_dir = crate_store.clone();
        } else if let Some(rest) = manifest_dir.strip_prefix(&old_prefix) {
            u.manifest_dir = format!("{}{}", crate_store, rest);
        }
        // Record the crate's project-src-relative directory for local crates
        // so the remap builder can keep their diagnostics rooted at the real
        // workspace path. Vendored crates slice off the vendor dir, not the
        // project-src root the path_prefix_remaps describe, so they keep None.
        if is_local {
            u.sliced_crate_rel = Some(crate_rel.clone());
        }
        unit_src_store[i] = crate_store;
    }
    Ok(unit_src_store)
}

#[cfg(test)]
mod slice_tests {
    use super::*;
    use std::path::Path;

    fn unit(is_local: bool, kind: UnitKind, crate_rel: &str, root: &str) -> NixUnit {
        NixUnit {
            key: "k".to_string(),
            drv_name: format!("{crate_rel}-drv"),
            kind,
            source_file: format!("{root}/{crate_rel}/src/lib.rs"),
            crate_name: crate_rel.replace('-', "_"),
            crate_types: vec!["lib".to_string()],
            edition: "2021".to_string(),
            features: vec![],
            dep_extern: vec![],
            all_dep_keys: vec![],
            build_script_dep: None,
            build_script_compile_key: None,
            manifest_dir: format!("{root}/{crate_rel}"),
            original_manifest_dir: String::new(),
            cargo_envs: vec![],
            extra_filename: String::new(),
            needs_linker: false,
            is_local,
            links: None,
            links_dep_keys: vec![],
            is_root: false,
            target_name: String::new(),
            for_host: false,
            compile_test: false,
            self_contained_build_script: false,
            sliced_crate_rel: None,
            drv_path: None,
        }
    }

    /// The real per-crate content-addressed add, minus the store insertion:
    /// NAR-hash the crate subtree. Used so the test exercises actual addressing.
    fn ca_add(p: &str) -> Result<String> {
        let nar = crate::nar::serialize_nar(Path::new(p), None)?;
        let name = Path::new(p).file_name().unwrap().to_string_lossy();
        Ok(crate::nar::compute_nar_store_path(
            &format!("{name}-src"),
            &nar,
        ))
    }

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    /// Model the cargo-vendor-dir symlink farm: `{vendor}/{name}` is a symlink
    /// to a content-addressed store-like directory `{store}/{hash}-{name}`, so
    /// canonicalize resolves to the per-crate target and a content change moves
    /// the target the way a real CA store path moves.
    fn vendor_symlink(vendor: &str, store: &str, name: &str, body: &str) {
        let target = Path::new(store)
            .join(format!("{:x}-{name}", md5_like(body)))
            .join(name);
        write(
            &target.join("Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\n"),
        );
        write(&target.join("src/lib.rs"), body);
        let link = Path::new(vendor).join(name);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link).unwrap();
    }

    /// Tiny content discriminator for the test's fake CA store names.
    fn md5_like(s: &str) -> u64 {
        let mut h = 1469598103934665603u64;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    /// Changes 1 and 2 wiring end-to-end with real per-crate NAR addressing.
    ///
    /// Change 1: editing local crate-b leaves crate-a's compile unit's assigned
    /// store byte-identical; the local build-script-run unit keeps the whole
    /// tree.
    ///
    /// Change 2: a vendored dep's units are sliced off the aggregate vendor dir
    /// onto a per-crate store, and bumping an unrelated vendored crate leaves it
    /// byte-identical — the lock-axis decoupling.
    #[test]
    fn assign_decouples_local_and_vendored_crates() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src").to_string_lossy().to_string();
        let vendor = tmp.path().join("vendor").to_string_lossy().to_string();
        let store = tmp.path().join("store").to_string_lossy().to_string();
        write(
            &Path::new(&src).join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\n",
        );
        write(
            &Path::new(&src).join("crate-a/src/lib.rs"),
            "pub fn a() {}\n",
        );
        write(
            &Path::new(&src).join("crate-b/Cargo.toml"),
            "[package]\nname = \"crate-b\"\n",
        );
        write(
            &Path::new(&src).join("crate-b/src/lib.rs"),
            "pub fn b() {}\n",
        );
        vendor_symlink(&vendor, &store, "serde", "pub fn s() {}\n");
        vendor_symlink(&vendor, &store, "once_cell", "pub fn o() {}\n");

        let mk = || {
            let mut self_contained = unit(true, UnitKind::BuildScriptRun, "crate-b", &src);
            self_contained.self_contained_build_script = true;
            vec![
                unit(true, UnitKind::Compile, "crate-a", &src),
                unit(true, UnitKind::Compile, "crate-b", &src),
                unit(true, UnitKind::BuildScriptRun, "crate-a", &src),
                unit(false, UnitKind::Compile, "serde", &vendor),
                unit(false, UnitKind::BuildScriptRun, "once_cell", &vendor),
                self_contained, // index 5: Change 3 — local self-contained build-script-run.
            ]
        };

        let mut units = mk();
        let s1 = assign_per_crate_src_stores(&mut units, &src, &vendor, ca_add).unwrap();

        // Local compile unit: sliced off the whole tree, paths rewritten onto it.
        assert_ne!(
            s1[0], src,
            "crate-a compile unit must be sliced off the whole tree"
        );
        assert!(
            units[0].source_file.starts_with(&s1[0]),
            "source_file rewritten onto per-crate store"
        );
        assert_eq!(
            units[0].manifest_dir, s1[0],
            "manifest_dir rewritten onto per-crate store"
        );
        // A sliced local unit records its project-src-relative dir so the
        // remap builder can keep diagnostics rooted at the real workspace path.
        assert_eq!(
            units[0].sliced_crate_rel.as_deref(),
            Some("crate-a"),
            "sliced local unit must record its crate_rel"
        );
        // Local build-script-run unit keeps the whole tree (sibling reads).
        assert_eq!(
            s1[2], src,
            "local build-script-run unit keeps the whole-tree src_store"
        );
        assert_eq!(
            units[2].sliced_crate_rel, None,
            "non-sliced local unit must not record a crate_rel"
        );
        // Vendored units slice off the vendor dir, not the project-src root the
        // path_prefix_remaps describe, so they keep None (their remap, if any,
        // must not be crate_rel-adjusted against the project-src replacement).
        assert_eq!(
            units[3].sliced_crate_rel, None,
            "vendored sliced unit must not record a crate_rel"
        );
        assert_eq!(
            units[2].source_file,
            format!("{src}/crate-a/src/lib.rs"),
            "local build-script unit not rewritten"
        );
        // Change 3: a local build-script-run that opts into self-contained IS
        // sliced to its own per-crate source, so it stops re-keying on unrelated
        // workspace edits.
        assert_ne!(
            s1[5], src,
            "self-contained local build-script-run unit must be sliced off the whole tree"
        );
        assert_eq!(
            units[5].manifest_dir, s1[5],
            "self-contained build-script unit manifest_dir rewritten onto per-crate store"
        );
        // Vendored units: resolved through the symlink farm onto their per-crate
        // store target, including the vendored build-script-run unit (a vendored
        // crate is self-contained). The assigned store is the symlink TARGET, so
        // the unit's paths point at real content, not the unmounted symlink.
        assert!(
            s1[3].starts_with(&store),
            "vendored serde resolves to its per-crate store target"
        );
        assert_eq!(
            units[3].manifest_dir, s1[3],
            "vendored serde manifest_dir rewritten onto per-crate target"
        );
        assert!(
            units[3].source_file.starts_with(&s1[3]),
            "vendored serde source_file rewritten onto its target"
        );
        assert!(
            s1[4].starts_with(&store),
            "vendored once_cell build-script-run unit resolves to its target"
        );
        assert_eq!(
            units[4].manifest_dir, s1[4],
            "vendored once_cell manifest_dir rewritten onto per-crate target"
        );

        let a_store_before = s1[0].clone();
        let b_store_before = s1[1].clone();
        let serde_store_before = s1[3].clone();
        let once_cell_before = s1[4].clone();

        // Edit ONLY local crate-b and vendored once_cell; re-run on fresh units.
        write(
            &Path::new(&src).join("crate-b/src/lib.rs"),
            "pub fn b() { let _ = 1; }\n",
        );
        vendor_symlink(&vendor, &store, "once_cell", "pub fn o() { let _ = 1; }\n");
        let mut units2 = mk();
        let s2 = assign_per_crate_src_stores(&mut units2, &src, &vendor, ca_add).unwrap();

        assert_eq!(
            s2[0], a_store_before,
            "editing crate-b must NOT change crate-a's assigned source store"
        );
        assert_ne!(
            s2[1], b_store_before,
            "crate-b's own assigned source store changes"
        );
        assert_eq!(
            s2[3], serde_store_before,
            "bumping once_cell must NOT change serde's per-crate vendor target"
        );
        assert_ne!(
            s2[4], once_cell_before,
            "once_cell's own per-crate vendor target changes"
        );
    }
}

#[cfg(test)]
mod unit_setup_tests {
    use super::*;

    fn unit(pkg: &str, target: &str, kind: UnitKind, is_local: bool) -> NixUnit {
        NixUnit {
            key: format!("{pkg}-{target}-{kind:?}"),
            drv_name: format!("{pkg}-{target}"),
            kind,
            source_file: String::new(),
            crate_name: target.replace('-', "_"),
            crate_types: vec!["lib".to_string()],
            edition: "2021".to_string(),
            features: vec![],
            dep_extern: vec![],
            all_dep_keys: vec![],
            build_script_dep: None,
            build_script_compile_key: None,
            manifest_dir: String::new(),
            original_manifest_dir: String::new(),
            cargo_envs: vec![("CARGO_PKG_NAME".into(), pkg.into())],
            extra_filename: String::new(),
            needs_linker: false,
            is_local,
            links: None,
            links_dep_keys: vec![],
            is_root: false,
            target_name: String::new(),
            for_host: false,
            compile_test: false,
            self_contained_build_script: false,
            sliced_crate_rel: None,
            drv_path: None,
        }
    }

    fn rule(package: &str, kinds: &str, targets: &str, script: &str) -> UnitSetupRule {
        // Build via the JSON path so the tests exercise the same
        // deserialisation `CARGO_SCHNEE_UNIT_SETUP` goes through.
        let mut json = format!(r#"{{"package": "{package}", "script": "{script}""#);
        if !kinds.is_empty() {
            json.push_str(&format!(r#", "kinds": {kinds}"#));
        }
        if !targets.is_empty() {
            json.push_str(&format!(r#", "targets": {targets}"#));
        }
        json.push('}');
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn default_kinds_cover_macro_expansion_kinds_only() {
        // The default kind set covers compile, check, test-compile and
        // doc — but neither build-script kind.
        let rules = [rule("my-backend", "", "", "/nix/store/s")];
        for kind in [
            UnitKind::Compile,
            UnitKind::Check,
            UnitKind::TestCompile,
            UnitKind::Doc,
        ] {
            let u = unit("my-backend", "my-backend", kind, true);
            assert_eq!(unit_setup_scripts(&u, &rules), ["/nix/store/s"]);
        }
        for kind in [UnitKind::BuildScriptCompile, UnitKind::BuildScriptRun] {
            let u = unit("my-backend", "build-script-build", kind, true);
            assert!(unit_setup_scripts(&u, &rules).is_empty());
        }
    }

    #[test]
    fn kind_filter_leaves_other_kinds_untouched() {
        // kinds = ["check"]: the same package's doc and build-script
        // units get no scripts.
        let rules = [rule("pkg", r#"["check"]"#, "", "/nix/store/s")];
        let hit = unit("pkg", "pkg", UnitKind::Check, true);
        assert_eq!(unit_setup_scripts(&hit, &rules), ["/nix/store/s"]);
        for kind in [
            UnitKind::Compile,
            UnitKind::Doc,
            UnitKind::TestCompile,
            UnitKind::BuildScriptCompile,
            UnitKind::BuildScriptRun,
        ] {
            let miss = unit("pkg", "pkg", kind, true);
            assert!(unit_setup_scripts(&miss, &rules).is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn build_script_kinds_matchable_when_named_explicitly() {
        let rules = [rule(
            "pkg",
            r#"["build-script-compile", "build-script-run"]"#,
            "",
            "/nix/store/s",
        )];
        for kind in [UnitKind::BuildScriptCompile, UnitKind::BuildScriptRun] {
            let u = unit("pkg", "build-script-build", kind, true);
            assert_eq!(unit_setup_scripts(&u, &rules), ["/nix/store/s"]);
        }
    }

    #[test]
    fn target_filter_matches_named_targets_only() {
        // A package with a lib target and a bin target: restricting to
        // the lib leaves the bin unit without scripts.  Target names
        // compare with `-` collapsed to `_` because `crate_name` derives
        // from `unit.target.name()` that way.
        let rules = [rule(
            "my-backend",
            r#"["check"]"#,
            r#"["my-backend"]"#,
            "/nix/store/s",
        )];
        let lib = unit("my-backend", "my-backend", UnitKind::Check, true);
        let bin = unit("my-backend", "my-ctl", UnitKind::Check, true);
        assert_eq!(unit_setup_scripts(&lib, &rules), ["/nix/store/s"]);
        assert!(unit_setup_scripts(&bin, &rules).is_empty());
    }

    #[test]
    fn package_filter_matches_vendored_packages() {
        // A rule naming a vendored package injects into that package's
        // units and no others — forking its unit caches is the intended
        // meaning.
        let rules = [rule("serde", "", "", "/nix/store/s")];
        let vendored = unit("serde", "serde", UnitKind::Compile, false);
        let sibling = unit("serde_json", "serde_json", UnitKind::Compile, false);
        let local = unit("my-backend", "my-backend", UnitKind::Compile, true);
        assert_eq!(unit_setup_scripts(&vendored, &rules), ["/nix/store/s"]);
        assert!(unit_setup_scripts(&sibling, &rules).is_empty());
        assert!(unit_setup_scripts(&local, &rules).is_empty());
    }

    #[test]
    fn star_matches_every_package() {
        let rules = [rule("*", "", "", "/nix/store/s")];
        let vendored = unit("serde", "serde", UnitKind::Compile, false);
        let local = unit("my-backend", "my-backend", UnitKind::Compile, true);
        assert_eq!(unit_setup_scripts(&vendored, &rules), ["/nix/store/s"]);
        assert_eq!(unit_setup_scripts(&local, &rules), ["/nix/store/s"]);
    }

    #[test]
    fn matching_rules_contribute_in_list_order() {
        let rules = [
            rule("*", "", "", "/nix/store/first"),
            rule("pkg", "", "", "/nix/store/second"),
        ];
        let u = unit("pkg", "pkg", UnitKind::Check, true);
        assert_eq!(
            unit_setup_scripts(&u, &rules),
            ["/nix/store/first", "/nix/store/second"]
        );
    }

    #[test]
    fn empty_rule_list_matches_nothing() {
        let u = unit("pkg", "pkg", UnitKind::Check, true);
        assert!(unit_setup_scripts(&u, &[]).is_empty());
    }

    #[test]
    fn unmatched_rules_reported_by_index() {
        // A typo'd package name and a kind absent from the plan are both
        // flagged; the rule that matches is not.
        let rules = [
            rule("my-bakend", "", "", "/nix/store/typo"),
            rule("my-backend", "", "", "/nix/store/hit"),
            rule(
                "my-backend",
                r#"["test-compile"]"#,
                "",
                "/nix/store/wrong-kind",
            ),
        ];
        let units = [unit("my-backend", "my-backend", UnitKind::Check, true)];
        assert_eq!(unmatched_unit_setup_rules(&units, &rules), [0, 2]);
    }

    #[test]
    fn all_matching_rules_report_nothing() {
        let rules = [rule("*", "", "", "/nix/store/s")];
        let units = [unit("pkg", "pkg", UnitKind::Compile, true)];
        assert!(unmatched_unit_setup_rules(&units, &rules).is_empty());
    }

    #[test]
    fn unknown_kind_is_rejected_at_parse_time() {
        let err = serde_json::from_str::<Vec<UnitSetupRule>>(
            r#"[{"package": "p", "script": "/nix/store/s", "kinds": ["chekc"]}]"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown unit kind"));
    }

    #[test]
    fn unknown_rule_field_is_rejected_at_parse_time() {
        assert!(
            serde_json::from_str::<Vec<UnitSetupRule>>(
                r#"[{"package": "p", "script": "/nix/store/s", "target": ["oops"]}]"#,
            )
            .is_err()
        );
    }
}

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
    // Ordered unit setup rules from `CARGO_SCHNEE_UNIT_SETUP`.  Each
    // matching rule's script is sourced inside the unit's sandbox right
    // before the driver invocation; non-matching units are byte-identical
    // to a run without rules.
    unit_setup: &[UnitSetupRule],
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
    // Declared feature-resolution scope, already read from the workspace
    // manifest by the caller.  Applies identically to the fresh and the
    // cached path: `is_root` and `target_name` are serialised into the
    // cache entry, so the cached graph is a whole-scope graph that still
    // has to be narrowed to the roots this invocation asked for.
    resolution: Option<&ResolutionScope>,
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
    let (mut nix_units, cfg_envs, host_cfg_envs) =
        if let Some((old_src_store, mut units)) = cached_units {
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
                resolution,
            )?
        };
    // Both branches above yield the whole-scope graph when a scope is
    // declared — the cached one because `is_root` is serialised as it was
    // for the scope, the fresh one because `fresh_unit_graph` hands back
    // every root cargo resolved.  Narrowing therefore has to happen here,
    // on the joined path, or a cache hit and a cache miss would plan
    // different unit sets.
    if let Some(scope) = resolution {
        nix_units = narrow_to_requested_roots(nix_units, packages, exclude, scope)?;
    }
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
    let (bash_path, bash_store) = which_bash()?;
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

    // The daemon connection serves both the closure queries below and
    // derivation registration later; connect once, up front. Verify
    // mode skips it: that path always re-adds via the CLI to compare
    // paths.
    let mut daemon: Option<NixDaemonConn> = if !verify_drv_paths {
        match NixDaemonConn::connect() {
            Ok(conn) => {
                info!("Connected to Nix daemon");
                Some(conn)
            }
            Err(e) => {
                info!(
                    "Cannot connect to Nix daemon ({}), falling back to the nix CLI",
                    e
                );
                None
            }
        }
    } else {
        None // verify mode always uses nix derivation add
    };

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

        // Preferred path: BFS over the daemon connection. Exec-ing the
        // nix CLI once per root costs more in dynamic-linker relocation
        // than the actual store work (~40% of all planner CPU samples
        // in profiles); the same information is one wopQueryPathInfo
        // round trip per closure path, memoized across roots.
        let mut remaining: Vec<&str> = Vec::new();
        if let Some(conn) = daemon.as_mut() {
            let mut refs_memo: HashMap<String, Vec<String>> = HashMap::new();
            for store_path in &uncached {
                match conn.query_closure(store_path, &mut refs_memo) {
                    Ok(closure) => {
                        closure_cache.insert(store_path.to_string(), closure);
                    }
                    Err(e) => {
                        info!(
                            "Daemon closure query failed for {}: {}; falling back to nix-store -qR",
                            store_path, e
                        );
                        remaining.push(store_path);
                    }
                }
            }
        } else {
            remaining = uncached.clone();
        }

        // Fallback: query the leftover closures via CLI spawns, in parallel.
        let results: Vec<(String, Result<Vec<String>>)> = std::thread::scope(|s| {
            let handles: Vec<_> = remaining
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

    // Pre-flight: a unitSetup rule matching no unit is usually a typo'd
    // package or target name and would otherwise no-op silently.
    for i in unmatched_unit_setup_rules(&nix_units, unit_setup) {
        let rule = &unit_setup[i];
        tracing::warn!(
            "unitSetup rule for package {:?} (script {}) matched no units in \
             this plan. Check the package and target names — targets compare \
             with `-` collapsed to `_`. A rule list shared between build, \
             test, and clippy packages can legitimately match nothing in one \
             of them.",
            rule.package,
            rule.script,
        );
    }

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

    // Changes 1 and 2: slice each local compile/test/doc unit's source — and each
    // vendored dep's units — to their own per-crate NAR (see
    // assign_per_crate_src_stores), decoupling local units from sibling-crate
    // edits and vendored units from unrelated `Cargo.lock` bumps. In host mode
    // this runs against the live daemon; in the planner derivation it uses
    // recursive-nix.
    let vendor_str = vendor_dir.to_string_lossy().to_string();
    let unit_src_store = assign_per_crate_src_stores(&mut nix_units, &src_str, &vendor_str, |p| {
        crate::add_to_nix_store(p)
    })?;

    // Phase 1: construct every unit's derivation JSON, ATerm bytes, and
    // `.drv` store path, level by level. Paths are computed client-side,
    // so construction never touches the daemon: each level's computed
    // paths seed `dep_drv_map` for the next level's inputDrvs (a
    // registration mismatch aborts the run before the plan is used, so
    // seeding ahead of registration is safe). Units within a level are
    // independent and are constructed in parallel.
    let mut levels_units: Vec<Vec<LevelUnit>> = Vec::with_capacity(topo_levels.len());
    for (level_idx, level) in topo_levels.iter().enumerate() {
        let _level_span =
            tracing::info_span!("construct_level", idx = level_idx, width = level.len(),).entered();
        // All deps are resolved from previous levels' computed paths.
        let construct_one = |i: usize| -> Result<LevelUnit> {
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
            let setup_scripts = unit_setup_scripts(&nix_units[i], unit_setup);
            let json = construct_derivation(
                &nix_units,
                i,
                &key_to_idx,
                &dep_drv_map,
                &bash_path,
                &bash_store,
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
                &unit_src_store[i],
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
                &setup_scripts,
            )?;
            tracing::debug!(
                "Adding derivation for {}: {}",
                nix_units[i].key,
                serde_json::to_string_pretty(&json)?
            );
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
        };

        let level_units: Vec<LevelUnit> = if parallel_jobs > 1 && level.len() > 1 {
            let n_workers = std::cmp::min(parallel_jobs, level.len());
            let chunks = chunk_round_robin(level.clone(), n_workers);
            std::thread::scope(|s| -> Result<Vec<LevelUnit>> {
                let handles: Vec<_> = chunks
                    .into_iter()
                    .map(|chunk| {
                        let f = &construct_one;
                        s.spawn(move || {
                            chunk.into_iter().map(f).collect::<Result<Vec<LevelUnit>>>()
                        })
                    })
                    .collect();
                let mut all = Vec::with_capacity(level.len());
                for h in handles {
                    let chunk_units = h
                        .join()
                        .map_err(|_| anyhow::anyhow!("construction worker panicked"))??;
                    all.extend(chunk_units);
                }
                Ok(all)
            })?
        } else {
            level
                .iter()
                .map(|&i| construct_one(i))
                .collect::<Result<_>>()?
        };

        for u in &level_units {
            dep_drv_map.insert(u.unit_key.clone(), u.drv_path.clone());
            nix_units[u.i].drv_path = Some(u.drv_path.clone());
        }
        levels_units.push(level_units);
    }

    // Phase 2: one validity probe for the entire DAG. Client-side path
    // computation needs no daemon confirmation between levels, so every
    // unit can be checked in a single `wopQueryValidPaths` round trip
    // instead of one per level. Skipped in `--verify-drv-paths` mode
    // (which always re-adds via the CLI to compare paths) and when the
    // daemon is unavailable (the per-unit path falls through to the CLI).
    let valid_paths: std::collections::HashSet<String> = if verify_drv_paths {
        std::collections::HashSet::new()
    } else if let Some(ref mut conn) = daemon {
        let paths: Vec<&str> = levels_units
            .iter()
            .flatten()
            .map(|u| u.drv_path.as_str())
            .collect();
        let _s = tracing::info_span!("query_valid_paths_batched", n = paths.len(),).entered();
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

    // Phase 3: register cache misses, still level by level — a drv's
    // referenced input drvs must be valid in the store before a drv
    // depending on them can be added. `dep_drv_map` and
    // `nix_units[.].drv_path` were already seeded with the computed
    // paths in phase 1, and `register_unit` verifies the daemon returns
    // exactly those paths, so registration is pure side effect here.
    for (level_idx, level_units) in levels_units.into_iter().enumerate() {
        let width = level_units.len();
        let _level_span =
            tracing::info_span!("register_level", idx = level_idx, width = width,).entered();

        // Verify mode short-circuits everything: every unit always re-adds
        // via the CLI so the in-process .drv path can be cross-checked
        // against Nix's. Always serial — `verify_drv_paths` is a debug
        // option, performance does not matter.
        if verify_drv_paths {
            for unit in level_units {
                let nix_path = nix_derivation_add(&unit.json)
                    .with_context(|| format!("Failed to add derivation for {}", unit.unit_key))?;
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
            }
            continue;
        }

        let misses: Vec<LevelUnit> = level_units
            .into_iter()
            .filter(|u| !valid_paths.contains(&u.drv_path))
            .collect();
        cache_hits += width - misses.len();

        if misses.is_empty() {
            continue;
        }

        let n_workers = std::cmp::min(parallel_jobs, misses.len());
        cache_misses += misses.len();

        if n_workers <= 1 {
            // Serial path: reuse the daemon connection across levels.
            for unit in misses {
                let path = register_unit(&mut daemon, &unit)?;
                info!("Added {} -> {}", unit.unit_key, path);
            }
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
            std::thread::scope(|s| -> Result<()> {
                let handles: Vec<_> = chunks
                    .into_iter()
                    .map(|chunk| {
                        s.spawn(move || -> Result<()> {
                            let mut conn = NixDaemonConn::connect().ok();
                            for unit in chunk {
                                let path = register_unit(&mut conn, &unit)?;
                                info!("Added {} -> {}", unit.unit_key, path);
                            }
                            Ok(())
                        })
                    })
                    .collect();
                for h in handles {
                    h.join()
                        .map_err(|_| anyhow::anyhow!("registration worker panicked"))??;
                }
                Ok(())
            })?;
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
                .map(|p| (p, u.target_name.clone(), u.kind))
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
    let (bash_path, bash_store) = which_bash()?;
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
    let mut target_names: Vec<String> = Vec::with_capacity(root_drvs.len());
    for (drv_path, target_name, _) in root_drvs {
        placeholders.push(downstream_placeholder(drv_path, "out")?);
        input_drvs.insert(
            drv_path.clone(),
            serde_json::json!({"outputs": ["out"], "dynamicOutputs": {}}),
        );
        target_names.push(target_name.clone());
    }

    // Aggregator build script: each root drv's realised output path
    // arrives via `rootOuts` (space-separated).  Symlink each into
    // `$out/root-N`.  Symlink rather than copy keeps the aggregator's
    // NAR small and avoids file-mode quirks; consumers walk the
    // symlinks transparently.
    //
    // Alongside each symlink, write `$out/root-N.target_name` text
    // files carrying cargo's canonical target name as reported by
    // `unit.target.name()`.  These are the single source of truth for
    // the `buildPackage` install step's renaming logic: it no longer
    // has to guess the canonical filename from the per-unit hash-
    // suffixed output, eliminating the `_→-` translation flaw that
    // corrupts bins with genuinely underscored target names.
    // `rootNames` is newline-separated so each entry can hold any
    // character a target name can; target names never contain
    // newlines.
    let script = format!(
        "set -e\n\
         {coreutils}/bin/mkdir -p $out\n\
         read -r -a __outs <<< \"$rootOuts\"\n\
         mapfile -t __names <<< \"$rootNames\"\n\
         i=0\n\
         while [ \"$i\" -lt \"${{#__outs[@]}}\" ]; do\n\
           {coreutils}/bin/ln -s \"${{__outs[$i]}}\" \"$out/root-$i\"\n\
           printf '%s' \"${{__names[$i]}}\" > \"$out/root-$i.target_name\"\n\
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
    env.insert(
        "rootNames".into(),
        serde_json::Value::String(target_names.join("\n")),
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

    #[test]
    fn git_sources_get_vendored_source_overrides() {
        // A lock with a default-branch git dep, a crates.io dep, and a
        // branch-pinned git dep. The offline planner needs a `[source."git+…"]`
        // redirect for each git source, or cargo tries to fetch it over the
        // (sandbox-forbidden) network.
        let lock = "\
[[package]]
name = \"euc\"
version = \"0.6.0\"
source = \"git+https://github.com/zesterer/euc#e8f7aeece8f7aeece8f7aeece8f7aeece8f7aeec\"

[[package]]
name = \"clipline\"
version = \"0.2.0\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"
checksum = \"deadbeefdeadbeef\"

[[package]]
name = \"pinned\"
version = \"2.0.0\"
source = \"git+https://example.com/pinned?branch=main#abc123abc123abc123abc123abc123abc123abcd\"
";
        let cfg = git_source_overrides(lock);

        // Default-branch git dep: keyed by its url, no ref line.
        assert!(cfg.contains("[source.\"git+https://github.com/zesterer/euc\"]"));
        assert!(cfg.contains("git = \"https://github.com/zesterer/euc\""));

        // Branch-pinned git dep: the `?branch=` ref is carried over.
        assert!(cfg.contains("[source.\"git+https://example.com/pinned?branch=main\"]"));
        assert!(cfg.contains("branch = \"main\""));

        // Both git sources redirect to vendored copies …
        assert_eq!(
            cfg.matches("replace-with = \"vendored-sources\"").count(),
            2
        );
        // … and the crates.io registry dep is left to the existing crates-io redirect.
        assert!(!cfg.contains("crates.io-index"));
    }
}

#[cfg(test)]
mod resolution_scope_tests {
    use super::*;

    fn manifest(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Cargo.toml");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    #[test]
    fn target_key_wins_over_default() {
        let (_d, p) = manifest(
            "[workspace.metadata.schnee.resolution]\n\
             default = \"workspace\"\n\
             x86_64-pc-windows-msvc = { packages = [\"a\", \"b\"] }\n",
        );
        let scope = read_resolution_scope(&p, "x86_64-pc-windows-msvc").unwrap();
        assert_eq!(scope.key, "x86_64-pc-windows-msvc");
        assert_eq!(
            scope.spec,
            ResolutionSpec::Packages(vec!["a".into(), "b".into()]),
        );
    }

    #[test]
    fn unknown_key_falls_back_to_default() {
        let (_d, p) = manifest("[workspace.metadata.schnee.resolution]\ndefault = \"workspace\"\n");
        let scope = read_resolution_scope(&p, "aarch64-unknown-linux-gnu").unwrap();
        assert_eq!(scope.key, "default");
        assert_eq!(scope.spec, ResolutionSpec::Workspace);
        assert_eq!(
            scope.manifest_key(),
            "workspace.metadata.schnee.resolution.default",
        );
    }

    #[test]
    fn exclude_subtracts_from_packages() {
        let (_d, p) = manifest(
            "[workspace.metadata.schnee.resolution]\n\
             default = { packages = [\"a\", \"b\", \"c\"], exclude = [\"b\"] }\n",
        );
        let scope = read_resolution_scope(&p, "default").unwrap();
        assert_eq!(
            scope.spec,
            ResolutionSpec::Packages(vec!["a".into(), "c".into()]),
        );
    }

    #[test]
    fn exclude_alone_opts_out_of_the_workspace() {
        let (_d, p) = manifest(
            "[workspace.metadata.schnee.resolution]\n\
             default = { exclude = [\"slow\"] }\n",
        );
        let scope = read_resolution_scope(&p, "default").unwrap();
        assert_eq!(scope.spec, ResolutionSpec::Exclude(vec!["slow".into()]));
    }

    /// A member declaring its own scope would reintroduce the divergence
    /// the scope exists to remove, so only the workspace table is read.
    #[test]
    fn package_table_is_not_read() {
        let (_d, p) = manifest("[package.metadata.schnee.resolution]\ndefault = \"workspace\"\n");
        let err = read_resolution_scope(&p, "default")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("[workspace.metadata.schnee.resolution]"),
            "{err}"
        );
    }

    #[test]
    fn missing_key_and_no_default_is_an_error() {
        let (_d, p) = manifest(
            "[workspace.metadata.schnee.resolution]\nx86_64-pc-windows-msvc = \"workspace\"\n",
        );
        let err = read_resolution_scope(&p, "aarch64-apple-darwin")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`default` key"), "{err}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let (_d, p) =
            manifest("[workspace.metadata.schnee.resolution]\ndefault = { members = [\"a\"] }\n");
        let err = read_resolution_scope(&p, "default")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn only_workspace_is_an_accepted_string() {
        let (_d, p) = manifest("[workspace.metadata.schnee.resolution]\ndefault = \"all\"\n");
        let err = read_resolution_scope(&p, "default")
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"workspace\""), "{err}");
    }

    #[test]
    fn cache_component_names_key_and_selection() {
        let scope = ResolutionScope {
            key: "x86_64-pc-windows-msvc".into(),
            spec: ResolutionSpec::Packages(vec!["a".into(), "b".into()]),
        };
        assert_eq!(
            scope.cache_component(),
            "scope:x86_64-pc-windows-msvc:packages=a,b",
        );
    }
}

#[cfg(test)]
mod narrow_tests {
    use super::*;

    fn scope() -> ResolutionScope {
        ResolutionScope {
            key: "x86_64-pc-windows-msvc".into(),
            spec: ResolutionSpec::Workspace,
        }
    }

    fn unit(key: &str, pkg: &str, kind: UnitKind, is_root: bool) -> NixUnit {
        NixUnit {
            key: key.to_string(),
            drv_name: key.to_string(),
            kind,
            source_file: String::new(),
            crate_name: pkg.replace('-', "_"),
            crate_types: vec!["lib".to_string()],
            edition: "2021".to_string(),
            features: vec![],
            dep_extern: vec![],
            all_dep_keys: vec![],
            build_script_dep: None,
            build_script_compile_key: None,
            manifest_dir: String::new(),
            original_manifest_dir: String::new(),
            cargo_envs: vec![("CARGO_PKG_NAME".to_string(), pkg.to_string())],
            extra_filename: String::new(),
            needs_linker: false,
            is_local: true,
            links: None,
            links_dep_keys: vec![],
            is_root,
            target_name: pkg.to_string(),
            for_host: false,
            compile_test: false,
            self_contained_build_script: false,
            sliced_crate_rel: None,
            drv_path: None,
        }
    }

    fn keys(units: &[NixUnit]) -> Vec<&str> {
        units.iter().map(|u| u.key.as_str()).collect()
    }

    /// Two scope roots sharing a dependency: asking for one keeps the
    /// shared dep and drops the sibling's exclusive subtree.
    #[test]
    fn siblings_are_pruned_but_shared_deps_survive() {
        let mut a = unit("a", "a", UnitKind::Compile, true);
        a.dep_extern = vec![("shared".into(), "shared".into())];
        let mut b = unit("b", "b", UnitKind::Compile, true);
        b.dep_extern = vec![
            ("shared".into(), "shared".into()),
            ("only_b".into(), "only-b".into()),
        ];
        let units = vec![
            a,
            b,
            unit("shared", "shared", UnitKind::Compile, false),
            unit("only-b", "only-b", UnitKind::Compile, false),
        ];
        let kept = narrow_to_requested_roots(units, &["a".to_string()], &[], &scope()).unwrap();
        assert_eq!(keys(&kept), vec!["a", "shared"]);
        assert!(kept[0].is_root);
        assert!(!kept[1].is_root);
    }

    /// `all_dep_keys` skips BuildScriptRun units and `links_dep_keys` is
    /// populated only on them, so a walk over either alone loses edges.
    #[test]
    fn links_deps_are_followed_off_build_script_runs() {
        let mut root = unit("root", "root", UnitKind::Compile, true);
        root.build_script_dep = Some("run".into());
        let mut run = unit("run", "root", UnitKind::BuildScriptRun, false);
        run.build_script_compile_key = Some("bsc".into());
        run.links_dep_keys = vec![("sys-run".into(), "z".into())];
        let units = vec![
            root,
            run,
            unit("bsc", "root", UnitKind::BuildScriptCompile, false),
            unit("sys-run", "z-sys", UnitKind::BuildScriptRun, false),
            unit("unrelated", "unrelated", UnitKind::Compile, false),
        ];
        let kept = narrow_to_requested_roots(units, &["root".to_string()], &[], &scope()).unwrap();
        assert_eq!(keys(&kept), vec!["root", "run", "bsc", "sys-run"]);
    }

    /// A links dep that resolves to nothing would silently cost the build
    /// script its `DEP_*` environment, so it is an error, not a skip.
    #[test]
    fn dangling_links_dep_is_an_error() {
        let mut root = unit("root", "root", UnitKind::Compile, true);
        root.build_script_dep = Some("run".into());
        let mut run = unit("run", "root", UnitKind::BuildScriptRun, false);
        run.links_dep_keys = vec![("gone".into(), "z".into())];
        let err = narrow_to_requested_roots(vec![root, run], &["root".to_string()], &[], &scope())
            .unwrap_err()
            .to_string();
        assert!(err.contains("DEP_*"), "{err}");
    }

    #[test]
    fn requesting_a_non_scope_package_names_the_manifest_key() {
        let units = vec![unit("a", "a", UnitKind::Compile, true)];
        let err = narrow_to_requested_roots(units, &["b".to_string()], &[], &scope())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("workspace.metadata.schnee.resolution.x86_64-pc-windows-msvc"),
            "{err}",
        );
    }

    /// No `-p` means "every root in the scope", so nothing is narrowed.
    #[test]
    fn empty_request_keeps_the_whole_scope() {
        let units = vec![
            unit("a", "a", UnitKind::Compile, true),
            unit("orphan", "orphan", UnitKind::Compile, false),
        ];
        let kept = narrow_to_requested_roots(units, &[], &[], &scope()).unwrap();
        assert_eq!(keys(&kept), vec!["a", "orphan"]);
    }

    /// Excluding everything that was requested selects nothing, which is an
    /// error.  Treating it as "no request" would invert the selection and
    /// build every root in the scope except the excluded one.
    #[test]
    fn requesting_and_excluding_the_same_package_is_an_error() {
        let units = vec![
            unit("a", "a", UnitKind::Compile, true),
            unit("b", "b", UnitKind::Compile, true),
        ];
        let err =
            narrow_to_requested_roots(units, &["a".to_string()], &["a".to_string()], &scope())
                .unwrap_err()
                .to_string();
        assert!(err.contains("selects nothing to build"), "{err}");
    }
}
