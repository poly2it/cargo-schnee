//! cargo-schnee: Nix-native Cargo build caching via dynamic derivations.
//!
//! `cargo schnee build` runs an in-process planner that captures rustc
//! invocations, generates per-unit CA derivations (registered via the Nix
//! daemon), and realises the root derivation with `nix-store --realise`.

mod diagnostics;
mod nar;
mod nix_encoding;
mod plan;
mod plan_nix;
mod shell;

use anyhow::{Context, Result};
use cargo::core::Workspace;
use cargo::util::command_prelude::UserIntent;
use cargo::util::context::GlobalContext;
use cargo::util::{Progress, ProgressStyle};
use clap::{Parser, Subcommand};
use log::LevelFilter;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "cargo", bin_name = "cargo")]
enum Cargo {
    /// Nix-native Cargo executor: replace dirty detection with Nix derivations
    Schnee(SchneeArgs),
}

#[derive(clap::Args)]
#[command(
    version,
    about = "Build Rust projects using Nix content-addressed derivations for caching"
)]
struct SchneeArgs {
    /// Use verbose output (-vv for extra detail)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Write build profiling report to the given path
    #[arg(long, global = true)]
    write_profile_to: Option<PathBuf>,

    /// Verify in-process .drv path computation against nix derivation add (debug)
    #[arg(long, global = true)]
    verify_drv_paths: bool,

    #[command(subcommand)]
    command: SchneeCommand,
}

#[derive(Subcommand)]
enum SchneeCommand {
    /// Build the project via dynamic derivations (nix build)
    Build {
        /// Path to Cargo.toml
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Use a pre-vendored dependency directory (nix store path) instead of
        /// running `cargo vendor`. Useful inside Nix sandbox builds where
        /// network access is unavailable.
        #[arg(long)]
        vendor_dir: Option<PathBuf>,
        /// Build artifacts in release mode, with optimizations
        #[arg(long)]
        release: bool,
        /// Build artifacts with the specified profile
        #[arg(long, conflicts_with = "release")]
        profile: Option<String>,
        /// Target triple for cross-compilation (e.g., aarch64-unknown-linux-gnu)
        #[arg(long)]
        target: Option<String>,
        /// Package(s) to build (can be specified multiple times)
        #[arg(short, long)]
        package: Vec<String>,
        /// Space or comma separated list of features to activate
        #[arg(long)]
        features: Vec<String>,
        /// Do not activate the `default` feature
        #[arg(long)]
        no_default_features: bool,
    },
    /// Build and run a binary target
    Run {
        /// Path to Cargo.toml
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Use a pre-vendored dependency directory
        #[arg(long)]
        vendor_dir: Option<PathBuf>,
        /// Build artifacts in release mode, with optimizations
        #[arg(long)]
        release: bool,
        /// Build artifacts with the specified profile
        #[arg(long, conflicts_with = "release")]
        profile: Option<String>,
        /// Target triple for cross-compilation
        #[arg(long)]
        target: Option<String>,
        /// Name of the binary target to run
        #[arg(long)]
        bin: Option<String>,
        /// Package(s) to build (can be specified multiple times)
        #[arg(short, long)]
        package: Vec<String>,
        /// Space or comma separated list of features to activate
        #[arg(long)]
        features: Vec<String>,
        /// Do not activate the `default` feature
        #[arg(long)]
        no_default_features: bool,
        /// Arguments passed to the binary after --
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Build and run test binaries
    Test {
        /// Path to Cargo.toml
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Use a pre-vendored dependency directory
        #[arg(long)]
        vendor_dir: Option<PathBuf>,
        /// Build artifacts in release mode, with optimizations
        #[arg(long)]
        release: bool,
        /// Build artifacts with the specified profile
        #[arg(long, conflicts_with = "release")]
        profile: Option<String>,
        /// Target triple for cross-compilation
        #[arg(long)]
        target: Option<String>,
        /// Package(s) to build (can be specified multiple times)
        #[arg(short, long)]
        package: Vec<String>,
        /// Space or comma separated list of features to activate
        #[arg(long)]
        features: Vec<String>,
        /// Do not activate the `default` feature
        #[arg(long)]
        no_default_features: bool,
        /// Arguments passed to the test harness after --
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Build and run benchmarks
    Bench {
        /// Path to Cargo.toml
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Use a pre-vendored dependency directory
        #[arg(long)]
        vendor_dir: Option<PathBuf>,
        /// Build artifacts in release mode, with optimizations
        #[arg(long)]
        release: bool,
        /// Build artifacts with the specified profile
        #[arg(long, conflicts_with = "release")]
        profile: Option<String>,
        /// Target triple for cross-compilation
        #[arg(long)]
        target: Option<String>,
        /// Package(s) to build (can be specified multiple times)
        #[arg(short, long)]
        package: Vec<String>,
        /// Space or comma separated list of features to activate
        #[arg(long)]
        features: Vec<String>,
        /// Do not activate the `default` feature
        #[arg(long)]
        no_default_features: bool,
        /// Arguments passed to the bench harness after --
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Dump the Nix derivation graph as a Mermaid flowchart
    Graph {
        /// Path to Cargo.toml
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Use a pre-vendored dependency directory
        #[arg(long)]
        vendor_dir: Option<PathBuf>,
        /// Build artifacts in release mode, with optimizations
        #[arg(long)]
        release: bool,
        /// Build artifacts with the specified profile
        #[arg(long, conflicts_with = "release")]
        profile: Option<String>,
        /// Target triple for cross-compilation
        #[arg(long)]
        target: Option<String>,
        /// Package(s) to build (can be specified multiple times)
        #[arg(short, long)]
        package: Vec<String>,
        /// Space or comma separated list of features to activate
        #[arg(long)]
        features: Vec<String>,
        /// Do not activate the `default` feature
        #[arg(long)]
        no_default_features: bool,
    },
    /// Extract and display the compilation unit graph
    Plan {
        /// Path to Cargo.toml
        #[arg(long)]
        manifest_path: Option<PathBuf>,
    },
    /// Internal: run inside planner derivation to capture invocations and emit .drv
    PlanNix {
        /// Path to the project source (in the Nix store)
        #[arg(long)]
        src: PathBuf,
        /// Path to vendored dependencies
        #[arg(long)]
        vendor_dir: PathBuf,
    },
}

fn resolve_manifest(manifest_path: &Option<PathBuf>) -> Result<PathBuf> {
    match manifest_path {
        Some(p) => {
            let p = p
                .canonicalize()
                .with_context(|| format!("cannot resolve manifest path '{}'", p.display()))?;
            if !p.exists() {
                anyhow::bail!(
                    "No Cargo.toml found at {}. Verify the file exists.",
                    p.display()
                );
            }
            Ok(p)
        }
        None => {
            let cwd = std::env::current_dir()?;
            let manifest = cwd.join("Cargo.toml");
            if !manifest.exists() {
                anyhow::bail!(
                    "No Cargo.toml found in '{}'. \
                     Run from a Cargo project root, or use --manifest-path <path/to/Cargo.toml>.",
                    cwd.display()
                );
            }
            Ok(manifest.canonicalize()?)
        }
    }
}

/// Read the binary target name from Cargo.toml (defaults to package name).
fn read_bin_target_name(manifest_path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(manifest_path)?;
    let doc: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;

    // Check [[bin]] first — if there's a bin target with a name, use it
    if let Some(name) = doc
        .get("bin")
        .and_then(|v| v.as_array())
        .and_then(|bins| bins.first())
        .and_then(|bin| bin.get("name"))
        .and_then(|v| v.as_str())
    {
        return Ok(name.to_string());
    }

    // Fall back to [package].name
    if let Some(name) = doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
    {
        return Ok(name.to_string());
    }

    anyhow::bail!(
        "Could not determine binary name from {}",
        manifest_path.display()
    )
}

/// Add a file or directory to the Nix store, returning the store path.
fn add_to_nix_store(path: &str) -> Result<String> {
    let output = Command::new("nix-store")
        .arg("--add")
        .arg(path)
        .output()
        .context("Failed to run nix-store --add")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("nix-store --add failed: {}", stderr);
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Strip `../` prefixes from a relative path to produce an in-tree name.
/// Mirrors `sanitiseName` in `buildPackage.nix`.
/// e.g. `../my-lib` → `my-lib`, `../../foo/bar` → `foo/bar`
fn sanitise_extra_source_name(rel_path: &str) -> Result<String> {
    let stripped = rel_path.replace("../", "");
    if stripped == rel_path || stripped.is_empty() {
        anyhow::bail!(
            "Extra source path must contain '../' components: {}",
            rel_path
        );
    }
    Ok(stripped)
}

/// Compute a relative path from `from` directory to `to` path.
/// Both must be absolute (or at least share a common prefix).
fn relative_path(from: &Path, to: &Path) -> PathBuf {
    use std::path::Component;

    let from_components: Vec<_> = from.components().collect();
    let to_components: Vec<_> = to.components().collect();

    // Find common prefix length
    let common = from_components
        .iter()
        .zip(to_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut result = PathBuf::new();
    // Go up for each remaining component in `from`
    for _ in &from_components[common..] {
        result.push(Component::ParentDir);
    }
    // Append remaining components of `to`
    for c in &to_components[common..] {
        result.push(c);
    }
    if result.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        result
    }
}

/// Discover path dependencies that live outside `project_dir`.
///
/// Uses cargo's `Workspace` to enumerate members, then parses each manifest
/// for `path = "..."` entries. Returns a map from canonical absolute path
/// to the sanitised in-tree name (e.g. `../my-lib` → `my-lib`).
fn find_external_path_deps(project_dir: &Path) -> Result<HashMap<PathBuf, String>> {
    let manifest_path = project_dir.join("Cargo.toml");
    let gctx = GlobalContext::default()?;
    let ws = Workspace::new(&manifest_path, &gctx)?;

    let project_canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());

    let mut manifests = vec![manifest_path.clone()];
    for pkg in ws.members() {
        let p = pkg.manifest_path().to_path_buf();
        if p != manifest_path {
            manifests.push(p);
        }
    }

    let mut external: HashMap<PathBuf, String> = HashMap::new();
    for manifest in &manifests {
        let content = match std::fs::read_to_string(manifest) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let doc: toml::Value = match toml::from_str(&content) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let manifest_dir = manifest.parent().unwrap();
        collect_external_path_deps(
            &doc,
            manifest_dir,
            &project_canonical,
            project_dir,
            &mut external,
        );
    }

    if !external.is_empty() {
        log::info!(
            "Found {} external path dep(s): {:?}",
            external.len(),
            external.values().collect::<Vec<_>>()
        );
    }
    Ok(external)
}

/// Walk all dependency tables in a parsed Cargo.toml and collect path deps
/// that resolve to directories outside `project_dir`.
fn collect_external_path_deps(
    doc: &toml::Value,
    manifest_dir: &Path,
    project_canonical: &Path,
    project_dir: &Path,
    external: &mut HashMap<PathBuf, String>,
) {
    let dep_sections = ["dependencies", "dev-dependencies", "build-dependencies"];

    // Top-level dep sections
    for section in &dep_sections {
        if let Some(table) = doc.get(section).and_then(|v| v.as_table()) {
            collect_external_from_table(
                table,
                manifest_dir,
                project_canonical,
                project_dir,
                external,
            );
        }
    }

    // [workspace.dependencies]
    if let Some(ws) = doc.get("workspace").and_then(|v| v.as_table())
        && let Some(table) = ws.get("dependencies").and_then(|v| v.as_table())
    {
        collect_external_from_table(
            table,
            manifest_dir,
            project_canonical,
            project_dir,
            external,
        );
    }

    // [target.*.{dependencies,dev-dependencies,build-dependencies}]
    if let Some(target) = doc.get("target").and_then(|v| v.as_table()) {
        for (_, target_val) in target {
            if let Some(target_table) = target_val.as_table() {
                for section in &dep_sections {
                    if let Some(table) = target_table.get(*section).and_then(|v| v.as_table()) {
                        collect_external_from_table(
                            table,
                            manifest_dir,
                            project_canonical,
                            project_dir,
                            external,
                        );
                    }
                }
            }
        }
    }

    // [patch.*]
    if let Some(patch) = doc.get("patch").and_then(|v| v.as_table()) {
        for (_, source_table) in patch {
            if let Some(table) = source_table.as_table() {
                collect_external_from_table(
                    table,
                    manifest_dir,
                    project_canonical,
                    project_dir,
                    external,
                );
            }
        }
    }
}

/// Inspect a single dependency table for path deps outside the project.
fn collect_external_from_table(
    table: &toml::value::Table,
    manifest_dir: &Path,
    project_canonical: &Path,
    project_dir: &Path,
    external: &mut HashMap<PathBuf, String>,
) {
    for (_, dep_val) in table {
        let path_str = match dep_val.get("path").and_then(|p| p.as_str()) {
            Some(s) => s,
            None => continue,
        };
        let resolved = manifest_dir.join(path_str);
        let canonical = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());
        if !canonical.starts_with(project_canonical) {
            let rel = relative_path(project_dir, &canonical);
            let rel_str = rel.to_string_lossy();
            if let Ok(sanitised) = sanitise_extra_source_name(&rel_str) {
                external.entry(canonical).or_insert(sanitised);
            }
        }
    }
}

/// Copy git-tracked files from an external source directory into `dest`.
fn copy_source_to_dest(source_dir: &Path, dest: &Path) -> Result<()> {
    let files = collect_git_files(source_dir)?;
    match files {
        Some(files) => {
            for file in &files {
                let src_path = source_dir.join(file);
                let dest_path = dest.join(file);
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if src_path.is_file() {
                    std::fs::copy(&src_path, &dest_path)
                        .with_context(|| format!("Failed to copy {}", src_path.display()))?;
                }
            }
            log::info!(
                "Extra source copy: {} files from {}",
                files.len(),
                source_dir.display()
            );
        }
        None => {
            copy_dir_excluding(source_dir, dest, &["target", ".git", ".direnv", "result"])?;
            log::info!("Extra source copy (no git): {}", source_dir.display());
        }
    }
    Ok(())
}

/// Rewrite path dependencies in all Cargo.toml files under `dest` so that
/// external deps point to their new in-tree locations.
fn rewrite_cargo_tomls(
    dest: &Path,
    project_dir: &Path,
    mappings: &HashMap<PathBuf, String>,
) -> Result<()> {
    let pattern = format!("{}/**/Cargo.toml", dest.display());
    let root_toml = dest.join("Cargo.toml");

    let mut toml_paths: Vec<PathBuf> = glob::glob(&pattern)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    if root_toml.exists() && !toml_paths.contains(&root_toml) {
        toml_paths.push(root_toml.clone());
    }

    let project_canonical = project_dir
        .canonicalize()
        .unwrap_or_else(|_| project_dir.to_path_buf());

    for toml_path in &toml_paths {
        let content = std::fs::read_to_string(toml_path)?;
        let mut doc: toml::Value = match toml::from_str(&content) {
            Ok(d) => d,
            Err(_) => continue,
        };

        // Determine the original manifest dir for path resolution
        let rel_in_dest = toml_path.strip_prefix(dest).unwrap_or(Path::new(""));
        let original_manifest_dir = project_canonical
            .join(rel_in_dest)
            .parent()
            .unwrap_or(&project_canonical)
            .to_path_buf();
        let dest_manifest_dir = toml_path.parent().unwrap();

        let mut changed = false;
        let dep_sections = ["dependencies", "dev-dependencies", "build-dependencies"];

        // Top-level dep sections
        for section in &dep_sections {
            if let Some(table) = doc.get_mut(section).and_then(|v| v.as_table_mut()) {
                changed |= rewrite_paths_in_table(
                    table,
                    &original_manifest_dir,
                    dest_manifest_dir,
                    dest,
                    mappings,
                );
            }
        }

        // [workspace.dependencies]
        if let Some(ws) = doc.get_mut("workspace").and_then(|v| v.as_table_mut())
            && let Some(table) = ws.get_mut("dependencies").and_then(|v| v.as_table_mut())
        {
            changed |= rewrite_paths_in_table(
                table,
                &original_manifest_dir,
                dest_manifest_dir,
                dest,
                mappings,
            );
        }

        // [target.*.deps]
        if let Some(target) = doc.get_mut("target").and_then(|v| v.as_table_mut()) {
            for (_, target_val) in target.iter_mut() {
                if let Some(target_table) = target_val.as_table_mut() {
                    for section in &dep_sections {
                        if let Some(table) = target_table
                            .get_mut(*section)
                            .and_then(|v| v.as_table_mut())
                        {
                            changed |= rewrite_paths_in_table(
                                table,
                                &original_manifest_dir,
                                dest_manifest_dir,
                                dest,
                                mappings,
                            );
                        }
                    }
                }
            }
        }

        // [patch.*]
        if let Some(patch) = doc.get_mut("patch").and_then(|v| v.as_table_mut()) {
            for (_, source_table) in patch.iter_mut() {
                if let Some(table) = source_table.as_table_mut() {
                    changed |= rewrite_paths_in_table(
                        table,
                        &original_manifest_dir,
                        dest_manifest_dir,
                        dest,
                        mappings,
                    );
                }
            }
        }

        if changed {
            std::fs::write(toml_path, toml::to_string(&doc)?)?;
        }
    }

    // Update [workspace].exclude in root Cargo.toml separately to avoid
    // symlink-resolved path comparison issues with glob results.
    update_workspace_exclude(&root_toml, mappings)?;

    Ok(())
}

/// Add external dep directory names to `[workspace].exclude` in the root
/// Cargo.toml so that workspace globs (e.g. `members = ["*"]`) don't try
/// to treat them as workspace members.
fn update_workspace_exclude(root_toml: &Path, mappings: &HashMap<PathBuf, String>) -> Result<()> {
    if !root_toml.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(root_toml)?;
    let mut doc: toml::Value = match toml::from_str(&content) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };

    let mut changed = false;
    if let Some(ws) = doc.get_mut("workspace").and_then(|v| v.as_table_mut()) {
        let exclude = ws
            .entry("exclude")
            .or_insert(toml::Value::Array(Vec::new()));
        if let Some(arr) = exclude.as_array_mut() {
            // Collect unique top-level directory names from sanitised paths.
            // e.g. "my-lib/sub" → "my-lib"
            let mut seen = std::collections::HashSet::new();
            for sanitised in mappings.values() {
                let top_dir = sanitised.split('/').next().unwrap_or(sanitised).to_string();
                if seen.insert(top_dir.clone()) {
                    let val = toml::Value::String(top_dir);
                    if !arr.contains(&val) {
                        arr.push(val);
                        changed = true;
                    }
                }
            }
        }
    }

    if changed {
        std::fs::write(root_toml, toml::to_string(&doc)?)?;
    }
    Ok(())
}

/// Rewrite `path = "..."` entries in a single dependency table.
/// Returns true if any entry was modified.
fn rewrite_paths_in_table(
    table: &mut toml::value::Table,
    original_manifest_dir: &Path,
    dest_manifest_dir: &Path,
    dest: &Path,
    mappings: &HashMap<PathBuf, String>,
) -> bool {
    let mut changed = false;
    for (_, dep_val) in table.iter_mut() {
        let dep_table = match dep_val.as_table_mut() {
            Some(t) => t,
            None => continue,
        };
        let path_str = match dep_table.get("path").and_then(|p| p.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let resolved = original_manifest_dir.join(&path_str);
        let canonical = resolved.canonicalize().unwrap_or(resolved);
        if let Some(sanitised) = mappings.get(&canonical) {
            let target_in_dest = dest.join(sanitised);
            let new_rel = relative_path(dest_manifest_dir, &target_in_dest);
            dep_table.insert(
                "path".into(),
                toml::Value::String(new_rel.to_string_lossy().into_owned()),
            );
            changed = true;
        }
    }
    changed
}

/// Add the project source to the Nix store, respecting .gitignore.
///
/// Uses libgit2 to discover tracked + untracked-but-not-ignored files,
/// then computes the NAR store path in-process. If the path already exists
/// in the store (warm build), skips the subprocess entirely. Otherwise,
/// copies files to a temp dir and runs `nix-store --add`.
///
/// When path dependencies outside `project_dir` are detected, they are
/// copied into the store tree and Cargo.toml paths are rewritten.
fn add_project_source_to_store(project_dir: &Path) -> Result<String> {
    // Collect allowed files via git2
    let allowed_files = collect_git_files(project_dir)?;

    // Detect external path dependencies
    let external_deps = find_external_path_deps(project_dir).unwrap_or_else(|e| {
        log::warn!("Failed to detect external path deps: {}", e);
        HashMap::new()
    });

    // Fast path: no external deps → try in-process NAR cache
    if external_deps.is_empty()
        && let Some(ref files) = allowed_files
    {
        match nar::serialize_nar(project_dir, Some(files)) {
            Ok(nar_data) => {
                let store_path = nar::compute_nar_store_path("project-src", &nar_data);
                if Path::new(&store_path).exists() {
                    log::info!("Source store path exists: {}", store_path);
                    return Ok(store_path);
                }
                log::info!("Source store path miss, falling back to subprocess");
            }
            Err(e) => {
                log::info!(
                    "NAR serialization failed ({}), falling back to subprocess",
                    e
                );
            }
        }
    }

    // Copy project files to temp dir
    let temp = tempfile::tempdir().context("Failed to create temp dir for source copy")?;
    let dest = temp.path().join("project-src");

    match &allowed_files {
        Some(files) => {
            for file in files {
                let src_path = project_dir.join(file);
                let dest_path = dest.join(file);
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if src_path.is_file() {
                    std::fs::copy(&src_path, &dest_path)
                        .with_context(|| format!("Failed to copy {}", src_path.display()))?;
                }
            }
            log::info!("Source copy: {} files via git2", files.len());
        }
        None => {
            log::info!("Source copy: falling back to hardcoded excludes (not a git repo)");
            copy_dir_excluding(project_dir, &dest, &["target", ".git", ".direnv", "result"])?;
        }
    }

    // Copy external path deps into the store tree and rewrite Cargo.toml paths
    if !external_deps.is_empty() {
        for (abs_path, sanitised) in &external_deps {
            let ext_dest = dest.join(sanitised);
            copy_source_to_dest(abs_path, &ext_dest)
                .with_context(|| format!("Failed to copy extra source: {}", abs_path.display()))?;
        }
        rewrite_cargo_tomls(&dest, project_dir, &external_deps)?;
    }

    add_to_nix_store(&dest.to_string_lossy())
}

/// Collect git-tracked + untracked-but-not-ignored files.
/// Returns None if not in a git repo (caller should use fallback excludes).
fn collect_git_files(project_dir: &Path) -> Result<Option<HashSet<PathBuf>>> {
    let repo = match git2::Repository::discover(project_dir) {
        Ok(r) => r,
        Err(_) => {
            log::info!("Not a git repo, using hardcoded excludes");
            return Ok(None);
        }
    };

    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow::anyhow!("Bare git repo not supported"))?;
    let rel_prefix = project_dir.strip_prefix(workdir).unwrap_or(Path::new(""));

    let mut files: HashSet<PathBuf> = HashSet::new();

    // Tracked files from the index
    let index = repo.index().context("Failed to read git index")?;
    for entry in index.iter() {
        let path = PathBuf::from(std::str::from_utf8(&entry.path)?);
        if let Ok(rel) = path.strip_prefix(rel_prefix) {
            files.insert(rel.to_path_buf());
        }
    }

    // Untracked-but-not-ignored files via status
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .exclude_submodules(true);
    if !rel_prefix.as_os_str().is_empty() {
        opts.pathspec(rel_prefix.to_string_lossy().as_ref());
    }
    let statuses = repo.statuses(Some(&mut opts))?;
    for entry in statuses.iter() {
        if entry.status().intersects(git2::Status::WT_NEW) {
            let path = PathBuf::from(entry.path().unwrap_or(""));
            if let Ok(rel) = path.strip_prefix(rel_prefix) {
                files.insert(rel.to_path_buf());
            }
        }
    }

    log::info!("Source: {} files via git2", files.len());
    Ok(Some(files))
}

/// Recursively copy a directory, skipping entries whose names match the exclude list.
/// Symlinks are skipped to avoid accidentally copying large nix store closures.
fn copy_dir_excluding(src: &Path, dest: &Path, exclude: &[&str]) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("Failed to read dir {}", src.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if exclude.iter().any(|e| *e == name_str.as_ref()) {
            continue;
        }
        let ft = entry.metadata()?.file_type();
        if ft.is_symlink() {
            continue;
        }
        let src_path = entry.path();
        let dest_path = dest.join(&name);
        if ft.is_dir() {
            copy_dir_excluding(&src_path, &dest_path, exclude)?;
        } else {
            std::fs::copy(&src_path, &dest_path)
                .with_context(|| format!("Failed to copy {}", src_path.display()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Build cache — skip vendoring + derivation registration on unchanged builds
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SchneeCache {
    /// SHA-256 of Cargo.lock → vendor nix store path
    #[serde(default)]
    vendor_lockfile_hash: Option<String>,
    #[serde(default)]
    vendor_store_path: Option<String>,
    /// Tool closure cache: store path → sorted list of closure paths.
    /// Keyed on individual nix store paths (e.g. rustc, cc, pkg-config deps).
    /// Invalidated per-entry: if a store path changes, its old entry is simply unused.
    #[serde(default)]
    tool_closures: HashMap<String, Vec<String>>,
    /// Unit graph cache: hash(Cargo.lock + Cargo.toml) → cached NixUnit vec.
    /// The unit graph depends only on manifest metadata and resolved deps, not source content.
    #[serde(default)]
    unit_graph_hash: Option<String>,
    #[serde(default)]
    unit_graph_src_store: Option<String>,
    #[serde(default)]
    unit_graph: Option<Vec<plan_nix::NixUnit>>,
    /// Cached CARGO_CFG_* env vars for the target (from rustc --print cfg).
    /// Validity is tied to unit_graph_hash (same target triple + toolchain).
    #[serde(default)]
    target_cfg_envs: Option<Vec<(String, String)>>,
}

struct CacheLock {
    _file: std::fs::File,
}

impl CacheLock {
    fn acquire(project_dir: &Path) -> Result<Self> {
        use fs2::FileExt;
        let dir = project_dir.join("target");
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join(".schnee-cache.lock"))
            .context("Failed to open cache lock file")?;
        file.lock_exclusive()
            .context("Failed to acquire cache lock")?;
        Ok(Self { _file: file })
    }
}

impl Drop for CacheLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._file);
    }
}

impl SchneeCache {
    fn load(project_dir: &Path) -> Self {
        let path = project_dir.join("target/.schnee-cache.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, project_dir: &Path) -> Result<()> {
        use tempfile::NamedTempFile;
        let dir = project_dir.join("target");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(".schnee-cache.json");
        let json = serde_json::to_string_pretty(self)?;
        let mut tmp =
            NamedTempFile::new_in(&dir).context("Failed to create temp file for cache")?;
        std::io::Write::write_all(&mut tmp, json.as_bytes())?;
        tmp.persist(&path)
            .context("Failed to atomically persist cache file")?;
        Ok(())
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let content =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&content)))
}

/// Hash all workspace Cargo.toml files (root + members) for cache keying.
/// For non-workspace projects, falls back to hashing just the root Cargo.toml.
fn hash_workspace_manifests(manifest_path: &Path, project_dir: &Path) -> Result<String> {
    let content = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    let doc: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;

    // Check for [workspace] members
    let members = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array());

    let mut hasher = Sha256::new();
    // Always include root manifest
    hasher.update(std::fs::read(manifest_path)?);

    if let Some(members) = members {
        // Resolve member paths and hash each member's Cargo.toml.
        let mut member_manifests: Vec<PathBuf> = Vec::new();
        for member in members {
            if let Some(pattern) = member.as_str() {
                let full_pattern = project_dir
                    .join(pattern)
                    .join("Cargo.toml")
                    .to_string_lossy()
                    .into_owned();
                match glob::glob(&full_pattern) {
                    Ok(paths) => {
                        for entry in paths.flatten() {
                            member_manifests.push(entry);
                        }
                    }
                    Err(e) => {
                        log::warn!("Invalid workspace member glob pattern '{}': {}", pattern, e);
                    }
                }
            }
        }
        // Sort for deterministic hashing
        member_manifests.sort();
        for manifest in &member_manifests {
            if let Ok(content) = std::fs::read(manifest) {
                hasher.update(&content);
            }
        }
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// Run cargo vendor and add result to the nix store.
fn vendor_dependencies(manifest_path: &Path) -> Result<String> {
    let vendor_dir = tempfile::tempdir().context("Failed to create temp dir for vendoring")?;
    let vendor_path = vendor_dir.path().join("vendor");
    let vendor_output = Command::new("cargo")
        .arg("vendor")
        .arg("--manifest-path")
        .arg(manifest_path)
        .arg(&vendor_path)
        .output()
        .context("Failed to run cargo vendor")?;
    if !vendor_output.status.success() {
        let stderr = String::from_utf8_lossy(&vendor_output.stderr);
        anyhow::bail!("cargo vendor failed: {}", stderr);
    }
    // cargo vendor may not create the directory for zero-dep projects
    if !vendor_path.exists() {
        std::fs::create_dir_all(&vendor_path)?;
    }
    add_to_nix_store(&vendor_path.to_string_lossy())
}

/// Read the package version from Cargo.toml's [package] section.
fn read_package_version(manifest_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(manifest_path).ok()?;
    let doc: toml::Value = toml::from_str(&content).ok()?;
    doc.get("package")
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Write a human-readable build profiling report.
///
/// Per-derivation duration is estimated from gaps between consecutive `building` events.
/// For sequential builds (typical for cargo dependency chains), this is accurate.
/// For parallel builds, durations are approximate.
#[allow(clippy::too_many_arguments)]
fn write_profile(
    path: &Path,
    manifest_path: &Path,
    project_dir: &Path,
    total: std::time::Duration,
    vendor: std::time::Duration,
    plan: std::time::Duration,
    build: std::time::Duration,
    copy: std::time::Duration,
    building_events: &[(Instant, String)],
    build_end: Instant,
    total_drv_count: Option<usize>,
) -> Result<()> {
    use std::fmt::Write;

    let pkg_name = read_bin_target_name(manifest_path).unwrap_or_else(|_| "unknown".into());
    let pkg_version = read_package_version(manifest_path).unwrap_or_else(|| "0.0.0".into());

    let date_str = Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    let mut report = String::new();
    writeln!(report, "cargo-schnee build profile")?;
    writeln!(report, "==========================")?;
    writeln!(report)?;
    writeln!(report, "Project: {} v{}", pkg_name, pkg_version)?;
    writeln!(report, "Path:    {}", project_dir.display())?;
    writeln!(report, "Date:    {}", date_str)?;
    writeln!(report, "Total:   {:.2}s", total.as_secs_f64())?;
    writeln!(report)?;

    let total_secs = total.as_secs_f64();
    writeln!(report, "Phase Breakdown")?;
    writeln!(report, "---------------")?;
    for (name, dur) in [
        ("Vendoring", vendor),
        ("Planning", plan),
        ("Building", build),
        ("Copying", copy),
    ] {
        let secs = dur.as_secs_f64();
        let pct = if total_secs > 0.0 {
            secs / total_secs * 100.0
        } else {
            0.0
        };
        writeln!(report, "  {:12} {:>7.2}s {:>5.1}%", name, secs, pct)?;
    }
    writeln!(report)?;

    // Compute per-derivation durations from consecutive building events.
    // Each derivation's duration = time from its `building` event to the next one (or build end).
    let built_count = building_events.len();
    let mut durations: Vec<(String, std::time::Duration)> = Vec::new();

    for i in 0..building_events.len() {
        let (start, ref drv) = building_events[i];
        let end = if i + 1 < building_events.len() {
            building_events[i + 1].0
        } else {
            build_end
        };
        durations.push((drv.clone(), end.saturating_duration_since(start)));
    }

    let cached = total_drv_count
        .unwrap_or(built_count)
        .saturating_sub(built_count);

    writeln!(report, "Build Phase Details")?;
    writeln!(report, "-------------------")?;
    writeln!(
        report,
        "  {} derivations built, {} cached",
        built_count, cached
    )?;

    if !durations.is_empty() {
        // Sort by duration descending (longest first)
        durations.sort_by(|a, b| b.1.cmp(&a.1));

        let formatted: Vec<(String, std::time::Duration)> = durations
            .iter()
            .map(|(drv, dur)| {
                let (pkg, version, kind) = shell::parse_drv_display(drv);
                let suffix = match kind {
                    shell::DrvKind::BuildScriptRun => " (build script run)",
                    shell::DrvKind::BuildScriptCompile => " (build script)",
                    shell::DrvKind::TestCompile => " (test)",
                    shell::DrvKind::Compile => "",
                };
                let label = if version.is_empty() {
                    format!("{}{}", pkg, suffix)
                } else {
                    format!("{} v{}{}", pkg, version, suffix)
                };
                (label, *dur)
            })
            .collect();

        let max_label = formatted.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
        writeln!(report)?;
        writeln!(
            report,
            "  {:<width$}  Duration",
            "Derivation",
            width = max_label
        )?;
        for (label, dur) in &formatted {
            writeln!(
                report,
                "  {:<width$}  {:>7.2}s",
                label,
                dur.as_secs_f64(),
                width = max_label
            )?;
        }
    }

    std::fs::write(path, &report)?;
    Ok(())
}

struct BuildResult {
    /// Root derivation paths with target names and unit kinds
    root_drvs: Vec<(String, String, plan_nix::UnitKind)>,
    /// target/<profile>/ directory
    target_debug: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn run_build_pipeline(
    manifest_path_opt: &Option<PathBuf>,
    vendor_dir: &Option<PathBuf>,
    release: bool,
    profile_opt: &Option<String>,
    target: &Option<String>,
    packages: &[String],
    features: &[String],
    no_default_features: bool,
    user_intent: UserIntent,
    verify_drv_paths: bool,
    verbose: u8,
    write_profile_to: &Option<PathBuf>,
    interrupted: &Arc<AtomicBool>,
) -> Result<BuildResult> {
    let start_time = Instant::now();
    let manifest_path = resolve_manifest(manifest_path_opt)?;
    let project_dir = manifest_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine project directory"))?;

    let profile = if release {
        plan_nix::ProfileConfig::release()
    } else if let Some(p) = profile_opt {
        match p.as_str() {
            "dev" => plan_nix::ProfileConfig::dev(),
            "release" => plan_nix::ProfileConfig::release(),
            _ => plan_nix::ProfileConfig {
                name: p.clone(),
                opt_level: "0",
                debug_info: true,
            },
        }
    } else {
        plan_nix::ProfileConfig::dev()
    };

    let target_config = match target {
        Some(t) => plan_nix::TargetConfig::with_target(t),
        None => plan_nix::TargetConfig::native(),
    };

    // Load build cache (locked to prevent concurrent cache corruption)
    let _cache_lock = CacheLock::acquire(project_dir)?;
    let mut cache = SchneeCache::load(project_dir);

    // Vendor dependencies — skip if Cargo.lock hasn't changed
    let vendor_start = Instant::now();
    let lockfile_path = project_dir.join("Cargo.lock");
    let lockfile_hash = hash_file(&lockfile_path)?;
    let vendor_store = if let Some(dir) = vendor_dir {
        let dir = dir
            .canonicalize()
            .with_context(|| format!("Cannot canonicalize vendor dir: {}", dir.display()))?;
        shell::status("Vendoring", &format!("using provided {}", dir.display()));
        dir.to_string_lossy().into_owned()
    } else if cache.vendor_lockfile_hash.as_deref() == Some(&lockfile_hash) {
        if let Some(ref cached_path) = cache.vendor_store_path {
            if Path::new(cached_path).exists() {
                shell::status("Vendoring", "dependencies (cached)");
                cached_path.clone()
            } else {
                vendor_dependencies(&manifest_path)?
            }
        } else {
            vendor_dependencies(&manifest_path)?
        }
    } else {
        shell::status("Vendoring", "dependencies...");
        vendor_dependencies(&manifest_path)?
    };
    cache.vendor_lockfile_hash = Some(lockfile_hash.clone());
    cache.vendor_store_path = Some(vendor_store.clone());

    // Add project source to the Nix store
    let src_store = add_project_source_to_store(project_dir)?;
    let vendor_duration = vendor_start.elapsed();

    // Plan: extract unit graph and register per-unit CA derivations directly.
    let plan_start = Instant::now();
    shell::status("Planning", "build...");

    // Check unit graph cache
    let manifest_hash = hash_workspace_manifests(&manifest_path, project_dir)?;
    let intent_str = match user_intent {
        UserIntent::Build => "build",
        UserIntent::Test => "test",
        UserIntent::Bench => "bench",
        _ => "build",
    };
    let packages_str = packages.join(",");
    let features_str = features.join(",");
    let unit_graph_key = format!(
        "{}:{}:{}:{}:{}:{}:{}:{}",
        lockfile_hash,
        manifest_hash,
        profile.name,
        target_config.target_triple,
        intent_str,
        packages_str,
        features_str,
        no_default_features,
    );
    let cached_units = if cache.unit_graph_hash.as_deref() == Some(&unit_graph_key) {
        if let (Some(old_src), Some(units)) = (&cache.unit_graph_src_store, &cache.unit_graph) {
            log::info!("Unit graph cache hit ({} units)", units.len());
            Some((old_src.clone(), units.clone()))
        } else {
            None
        }
    } else {
        None
    };

    let cached_cfg_envs = if cached_units.is_some() {
        cache.target_cfg_envs.clone()
    } else {
        None
    };

    // Resolve passthrough env vars for build-script derivations.
    // CARGO_SCHNEE_PASSTHRU_ENVS is a space-separated list of env var names
    // whose values should be forwarded into per-crate build-script derivations.
    let passthru_envs: Vec<(String, String)> = std::env::var("CARGO_SCHNEE_PASSTHRU_ENVS")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|name| std::env::var(name).ok().map(|val| (name.to_string(), val)))
        .collect();

    let (root_drvs, plan_units, cfg_envs) = plan_nix::run_plan_nix(
        Path::new(&src_store),
        Path::new(&vendor_store),
        verify_drv_paths,
        &mut cache.tool_closures,
        cached_units,
        cached_cfg_envs,
        &profile,
        &target_config,
        user_intent,
        packages,
        features,
        no_default_features,
        &passthru_envs,
    )?;

    // Update unit graph cache
    cache.unit_graph_hash = Some(unit_graph_key);
    cache.unit_graph_src_store = Some(src_store.clone());
    cache.unit_graph = Some(
        plan_units
            .iter()
            .map(|u| {
                let mut c = u.clone();
                c.clear_drv_path();
                c
            })
            .collect(),
    );
    cache.target_cfg_envs = Some(cfg_envs);

    let plan_duration = plan_start.elapsed();

    // Build all root derivations
    let project_pkg_name = read_bin_target_name(&manifest_path).ok();
    let mut cmd = Command::new("nix-store");
    cmd.arg("--realise");
    for (drv_path, _) in &root_drvs {
        cmd.arg(drv_path);
    }
    let mut child = cmd
        .env("NIX_CONFIG", "extra-experimental-features = ca-derivations")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn nix-store --realise")?;

    let stdout = child.stdout.take().context("stdout not piped")?;
    let stdout_thread = std::thread::spawn(move || {
        let mut content = String::new();
        let mut reader = stdout;
        reader.read_to_string(&mut content).ok();
        content
    });

    let stderr = child.stderr.take().context("stderr not piped")?;
    let reader = std::io::BufReader::new(stderr);
    let mut seen_pkgs: HashSet<String> = HashSet::new();
    let build_start = Instant::now();
    let mut building_events: Vec<(Instant, String)> = Vec::new();
    let mut total_drv_count: Option<usize> = None;

    let gctx = GlobalContext::default()?;
    let mut progress = Progress::with_style("Building", ProgressStyle::Ratio, &gctx);
    let mut diag_shell = cargo::core::shell::Shell::new();
    let src_store_prefix = format!("{}/", src_store);
    let project_dir_prefix = format!("{}/", project_dir.display());

    for line in reader.lines().map_while(Result::ok) {
        if interrupted.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Interrupted by signal");
        }
        let trimmed = line.trim();
        if let Some(drv_path) = shell::parse_building_line(trimmed) {
            building_events.push((Instant::now(), drv_path.to_string()));
            let (pkg, version, kind) = shell::parse_drv_display(drv_path);
            match kind {
                shell::DrvKind::Compile | shell::DrvKind::TestCompile => {
                    let display_key = format!("{}-{}", pkg, version);
                    if seen_pkgs.insert(display_key) {
                        let is_project = project_pkg_name.as_deref() == Some(&pkg);
                        let msg = if !version.is_empty() {
                            if is_project {
                                format!("{} v{} ({})", pkg, version, project_dir.display())
                            } else {
                                format!("{} v{}", pkg, version)
                            }
                        } else {
                            pkg.clone()
                        };
                        progress.clear();
                        shell::status("Compiling", &msg);
                    }
                }
                shell::DrvKind::BuildScriptRun if verbose > 0 => {
                    let msg = if !version.is_empty() {
                        format!("build script for {} v{}", pkg, version)
                    } else {
                        format!("build script for {}", pkg)
                    };
                    progress.clear();
                    shell::status("Running", &msg);
                }
                _ => {}
            }
            if let Some(total) = total_drv_count {
                let suffix = if !pkg.is_empty() {
                    format!(": {}", pkg)
                } else {
                    String::new()
                };
                let _ = progress.tick_now(building_events.len(), total, &suffix);
            }
        } else if (trimmed.starts_with("these ") || trimmed.starts_with("this "))
            && (trimmed.ends_with("will be built:")
                || trimmed.ends_with("will be fetched:")
                || trimmed.ends_with("will be fetched (0.00 MiB download, 0.00 MiB unpacked):"))
        {
            if total_drv_count.is_none() {
                if let Some(rest) = trimmed.strip_prefix("these ") {
                    total_drv_count = rest.split_whitespace().next().and_then(|n| n.parse().ok());
                } else {
                    total_drv_count = Some(1);
                }
            }
        } else if trimmed.starts_with("/nix/store/")
            || trimmed.starts_with("resolved derivation:")
            || trimmed.starts_with("warning: you did not specify")
            || trimmed.starts_with("copying path '")
        {
        } else if let Some(quoted) = trimmed.strip_prefix("> ") {
            progress.clear();
            diagnostics::emit_line(
                &mut diag_shell,
                quoted,
                &src_store_prefix,
                &project_dir_prefix,
            );
        } else if !trimmed.is_empty() {
            progress.clear();
            diagnostics::emit_line(
                &mut diag_shell,
                trimmed,
                &src_store_prefix,
                &project_dir_prefix,
            );
        }
    }
    progress.clear();

    let child_status = child.wait()?;
    if interrupted.load(Ordering::Relaxed) {
        anyhow::bail!("Interrupted by signal");
    }
    let stdout_content = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("stdout capture thread panicked"))?;

    if !child_status.success() {
        anyhow::bail!(
            "nix-store --realise failed (exit code: {}).\n\
             If you see 'ca-derivations' errors, ensure your nix.conf includes:\n  \
             experimental-features = nix-command flakes ca-derivations\n\
             Or set NIX_CONFIG=\"extra-experimental-features = ca-derivations\".\n\
             Run with -vv for detailed Nix build logs.",
            child_status
                .code()
                .map_or("signal".to_string(), |c| c.to_string())
        );
    }
    let out_paths: Vec<String> = stdout_content
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let build_end = Instant::now();
    let build_duration = build_end.duration_since(build_start);

    // Copy outputs to target/<profile>/
    let copy_start = Instant::now();
    let profile_dir = if profile.name == "dev" {
        "debug"
    } else {
        &profile.name
    };
    let target_debug = if target_config.is_cross() {
        project_dir
            .join("target")
            .join(&target_config.target_triple)
            .join(profile_dir)
    } else {
        project_dir.join("target").join(profile_dir)
    };
    std::fs::create_dir_all(&target_debug)?;

    for (idx, (_, target_name)) in root_drvs.iter().enumerate() {
        let out_path = out_paths.get(idx).ok_or_else(|| {
            anyhow::anyhow!(
                "Missing output path for root {} (got {} paths for {} roots)",
                target_name,
                out_paths.len(),
                root_drvs.len()
            )
        })?;

        let mut bin_file = None;
        for entry in std::fs::read_dir(out_path)? {
            let entry = entry?;
            let dest = target_debug.join(entry.file_name());
            if dest.exists()
                && let Err(e) = std::fs::remove_file(&dest)
            {
                log::debug!("Failed to remove old file {}: {}", dest.display(), e);
            }
            std::fs::copy(entry.path(), &dest).with_context(|| {
                format!(
                    "Failed to copy {} to {}",
                    entry.path().display(),
                    dest.display()
                )
            })?;
            {
                use std::os::unix::fs::PermissionsExt;
                let src_mode = std::fs::metadata(entry.path())?.permissions().mode();
                let is_exec = src_mode & 0o111 != 0;
                let new_mode = if is_exec { 0o755 } else { 0o644 };
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(new_mode))?;
            }
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&dest)?.permissions().mode();
                if mode & 0o111 != 0 {
                    bin_file = Some(dest);
                }
            }
        }
        if let Some(ref bin_path) = bin_file {
            let clean_dest = target_debug.join(target_name);
            if clean_dest != *bin_path {
                if clean_dest.exists()
                    && let Err(e) = std::fs::remove_file(&clean_dest)
                {
                    log::debug!(
                        "Failed to remove old binary {}: {}",
                        clean_dest.display(),
                        e
                    );
                }
                std::fs::hard_link(bin_path, &clean_dest)
                    .or_else(|_| std::fs::copy(bin_path, &clean_dest).map(|_| ()))
                    .with_context(|| format!("Failed to create {}", clean_dest.display()))?;
            }
        }
    }
    let copy_duration = copy_start.elapsed();

    // Replay cached diagnostics
    {
        let built_set: HashSet<&str> = building_events
            .iter()
            .map(|(_, drv)| drv.as_str())
            .collect();
        let cached_local_drvs: Vec<&str> = plan_nix::local_compile_drv_paths(&plan_units)
            .into_iter()
            .filter(|drv| !built_set.contains(drv))
            .collect();
        if !cached_local_drvs.is_empty() {
            let resolve_output = Command::new("nix-store")
                .arg("--realise")
                .args(&cached_local_drvs)
                .env("NIX_CONFIG", "extra-experimental-features = ca-derivations")
                .output();
            if let Ok(output) = resolve_output {
                let stdout = String::from_utf8_lossy(&output.stdout);
                for out_path in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    let diag_path = Path::new(out_path).join("diagnostics");
                    diagnostics::replay_diagnostics_from_file(
                        &mut diag_shell,
                        &diag_path,
                        &src_store_prefix,
                        &project_dir_prefix,
                    );
                }
            }
        }
    }

    let elapsed = start_time.elapsed();
    let profile_desc = match profile.name.as_str() {
        "dev" => "`dev` profile [unoptimized + debuginfo]".to_string(),
        "release" => "`release` profile [optimized]".to_string(),
        _ => format!("`{}` profile", profile.name),
    };
    shell::status(
        "Finished",
        &format!(
            "{} target(s) in {:.2}s",
            profile_desc,
            elapsed.as_secs_f64()
        ),
    );

    // Save cache
    if let Err(e) = cache.save(project_dir) {
        log::warn!("Failed to save build cache: {}", e);
    }

    if let Some(profile_path) = write_profile_to {
        write_profile(
            profile_path,
            &manifest_path,
            project_dir,
            elapsed,
            vendor_duration,
            plan_duration,
            build_duration,
            copy_duration,
            &building_events,
            build_end,
            total_drv_count,
        )?;
        shell::status(
            "Profiling",
            &format!("report written to {}", profile_path.display()),
        );
    }

    // Build root_drvs with UnitKind info
    let root_drvs_with_kind: Vec<(String, String, plan_nix::UnitKind)> = root_drvs
        .into_iter()
        .map(|(drv_path, target_name)| {
            let kind = plan_units
                .iter()
                .find(|u| u.drv_path.as_deref() == Some(&drv_path))
                .map(|u| u.kind.clone())
                .unwrap_or(plan_nix::UnitKind::Compile);
            (drv_path, target_name, kind)
        })
        .collect();

    Ok(BuildResult {
        root_drvs: root_drvs_with_kind,
        target_debug,
    })
}

fn cleanup_stale_temps() {
    let tmp_dir = std::env::temp_dir();
    if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("cargo-schnee-")
                && let Ok(metadata) = entry.metadata()
                && let Ok(modified) = metadata.modified()
                && let Ok(elapsed) = modified.elapsed()
                && elapsed > std::time::Duration::from_secs(3600)
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

fn main() -> Result<()> {
    let Cargo::Schnee(args) = Cargo::parse();

    let log_level = match args.verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };
    env_logger::Builder::new()
        .filter_module("cargo_schnee", log_level)
        .format_target(false)
        .format_timestamp(None)
        .init();

    let interrupted = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&interrupted))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&interrupted))?;

    // Clean up stale temp dirs from previous runs (e.g. after SIGKILL)
    cleanup_stale_temps();

    let verbose = args.verbose;
    let write_profile_to = args.write_profile_to;
    let verify_drv_paths = args.verify_drv_paths;

    match args.command {
        SchneeCommand::Build {
            ref manifest_path,
            ref vendor_dir,
            release,
            ref profile,
            ref target,
            ref package,
            ref features,
            no_default_features,
        } => {
            run_build_pipeline(
                manifest_path,
                vendor_dir,
                release,
                profile,
                target,
                package,
                features,
                no_default_features,
                UserIntent::Build,
                verify_drv_paths,
                verbose,
                &write_profile_to,
                &interrupted,
            )?;
        }
        SchneeCommand::Run {
            ref manifest_path,
            ref vendor_dir,
            release,
            ref profile,
            ref target,
            ref bin,
            ref package,
            ref features,
            no_default_features,
            ref args,
        } => {
            let result = run_build_pipeline(
                manifest_path,
                vendor_dir,
                release,
                profile,
                target,
                package,
                features,
                no_default_features,
                UserIntent::Build,
                verify_drv_paths,
                verbose,
                &write_profile_to,
                &interrupted,
            )?;

            // Find the binary to run
            let bin_roots: Vec<_> = result
                .root_drvs
                .iter()
                .filter(|(_, _, kind)| matches!(kind, plan_nix::UnitKind::Compile))
                .collect();

            let (_, target_name, _) = if let Some(bin_name) = bin {
                bin_roots
                    .iter()
                    .find(|(_, name, _)| name == bin_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no bin target named `{}`\navailable targets: {}",
                            bin_name,
                            bin_roots
                                .iter()
                                .map(|(_, n, _)| n.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?
            } else if bin_roots.len() == 1 {
                bin_roots[0]
            } else {
                anyhow::bail!(
                    "multiple binary targets found, use --bin to specify one: {}",
                    bin_roots
                        .iter()
                        .map(|(_, n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            };

            let binary_path = result.target_debug.join(target_name);
            shell::status("Running", &format!("`{}`", binary_path.display()));
            let status = Command::new(&binary_path)
                .args(args)
                .status()
                .with_context(|| format!("Failed to execute {}", binary_path.display()))?;
            std::process::exit(status.code().unwrap_or(1));
        }
        SchneeCommand::Test {
            ref manifest_path,
            ref vendor_dir,
            release,
            ref profile,
            ref target,
            ref package,
            ref features,
            no_default_features,
            ref args,
        } => {
            let result = run_build_pipeline(
                manifest_path,
                vendor_dir,
                release,
                profile,
                target,
                package,
                features,
                no_default_features,
                UserIntent::Test,
                verify_drv_paths,
                verbose,
                &write_profile_to,
                &interrupted,
            )?;

            // Find test binaries (TestCompile roots)
            let test_roots: Vec<_> = result
                .root_drvs
                .iter()
                .filter(|(_, _, kind)| matches!(kind, plan_nix::UnitKind::TestCompile))
                .collect();

            if test_roots.is_empty() {
                shell::status("Finished", "no test targets to run");
                return Ok(());
            }

            let mut any_failed = false;
            for (_, target_name, _) in &test_roots {
                let binary_path = result.target_debug.join(target_name);
                shell::status("Running", &format!("tests in `{}`", binary_path.display()));
                let status = Command::new(&binary_path)
                    .args(args)
                    .status()
                    .with_context(|| format!("Failed to execute {}", binary_path.display()))?;
                if !status.success() {
                    any_failed = true;
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        SchneeCommand::Bench {
            ref manifest_path,
            ref vendor_dir,
            release,
            ref profile,
            ref target,
            ref package,
            ref features,
            no_default_features,
            ref args,
        } => {
            let result = run_build_pipeline(
                manifest_path,
                vendor_dir,
                release,
                profile,
                target,
                package,
                features,
                no_default_features,
                UserIntent::Bench,
                verify_drv_paths,
                verbose,
                &write_profile_to,
                &interrupted,
            )?;

            // Find bench binaries (TestCompile roots — bench uses same compile mode)
            let bench_roots: Vec<_> = result
                .root_drvs
                .iter()
                .filter(|(_, _, kind)| matches!(kind, plan_nix::UnitKind::TestCompile))
                .collect();

            if bench_roots.is_empty() {
                shell::status("Finished", "no bench targets to run");
                return Ok(());
            }

            let mut any_failed = false;
            for (_, target_name, _) in &bench_roots {
                let binary_path = result.target_debug.join(target_name);
                shell::status(
                    "Running",
                    &format!("benchmarks in `{}`", binary_path.display()),
                );
                let mut bench_args = vec!["--bench".to_string()];
                bench_args.extend(args.iter().cloned());
                let status = Command::new(&binary_path)
                    .args(&bench_args)
                    .status()
                    .with_context(|| format!("Failed to execute {}", binary_path.display()))?;
                if !status.success() {
                    any_failed = true;
                }
            }
            if any_failed {
                std::process::exit(1);
            }
        }
        SchneeCommand::Graph {
            ref manifest_path,
            ref vendor_dir,
            release,
            ref profile,
            ref target,
            ref package,
            ref features,
            no_default_features,
        } => {
            let manifest_path = resolve_manifest(manifest_path)?;
            let project_dir = manifest_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Cannot determine project directory"))?;

            let profile_cfg = if release {
                plan_nix::ProfileConfig::release()
            } else if let Some(p) = profile {
                match p.as_str() {
                    "dev" => plan_nix::ProfileConfig::dev(),
                    "release" => plan_nix::ProfileConfig::release(),
                    _ => plan_nix::ProfileConfig {
                        name: p.clone(),
                        opt_level: "0",
                        debug_info: true,
                    },
                }
            } else {
                plan_nix::ProfileConfig::dev()
            };

            let target_config = match target {
                Some(t) => plan_nix::TargetConfig::with_target(t),
                None => plan_nix::TargetConfig::native(),
            };

            let vendor_store = match vendor_dir {
                Some(dir) => {
                    let dir = dir.canonicalize().with_context(|| {
                        format!("Cannot canonicalize vendor dir: {}", dir.display())
                    })?;
                    dir.to_string_lossy().into_owned()
                }
                None => vendor_dependencies(&manifest_path)?,
            };

            let src_store = add_project_source_to_store(project_dir)?;
            let mut closure_cache = HashMap::new();
            let (_, plan_units, _) = plan_nix::run_plan_nix(
                Path::new(&src_store),
                Path::new(&vendor_store),
                verify_drv_paths,
                &mut closure_cache,
                None,
                None,
                &profile_cfg,
                &target_config,
                UserIntent::Build,
                package,
                features,
                no_default_features,
                &[],
            )?;

            println!("{}", plan::format_mermaid_graph(&plan_units));
        }
        SchneeCommand::Plan { ref manifest_path } => {
            let manifest_path = resolve_manifest(manifest_path)?;
            let build_plan = plan::extract_build_plan(&manifest_path)?;
            plan::print_build_plan(&build_plan);
        }
        SchneeCommand::PlanNix {
            ref src,
            ref vendor_dir,
        } => {
            let mut closure_cache = HashMap::new();
            let default_profile = plan_nix::ProfileConfig::dev();
            let default_target = plan_nix::TargetConfig::native();
            let (root_drvs, _, _) = plan_nix::run_plan_nix(
                src,
                vendor_dir,
                verify_drv_paths,
                &mut closure_cache,
                None,
                None,
                &default_profile,
                &default_target,
                UserIntent::Build,
                &[],
                &[],
                false,
                &[],
            )?;
            // Output the root .drv paths
            for (drv_path, _) in &root_drvs {
                println!("{}", drv_path);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_bin_target_name_from_package() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"
"#,
        )
        .unwrap();
        assert_eq!(read_bin_target_name(&manifest).unwrap(), "my-app");
    }

    #[test]
    fn read_bin_target_name_from_bin() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "custom-bin"
path = "src/main.rs"
"#,
        )
        .unwrap();
        assert_eq!(read_bin_target_name(&manifest).unwrap(), "custom-bin");
    }

    #[test]
    fn read_bin_target_name_missing() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "[dependencies]\n").unwrap();
        assert!(read_bin_target_name(&manifest).is_err());
    }

    #[test]
    fn read_package_version_present() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"
[package]
name = "foo"
version = "1.2.3"
"#,
        )
        .unwrap();
        assert_eq!(read_package_version(&manifest), Some("1.2.3".to_string()));
    }

    #[test]
    fn read_package_version_missing() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(&manifest, "[dependencies]\n").unwrap();
        assert_eq!(read_package_version(&manifest), None);
    }

    #[test]
    fn sanitise_strips_dot_dot() {
        assert_eq!(sanitise_extra_source_name("../foo").unwrap(), "foo");
        assert_eq!(
            sanitise_extra_source_name("../../bar/baz").unwrap(),
            "bar/baz"
        );
    }

    #[test]
    fn sanitise_rejects_no_dot_dot() {
        assert!(sanitise_extra_source_name("foo").is_err());
        assert!(sanitise_extra_source_name("./foo").is_err());
    }

    #[test]
    fn relative_path_sibling() {
        let from = Path::new("/a/b/project");
        let to = Path::new("/a/b/sibling");
        assert_eq!(relative_path(from, to), PathBuf::from("../sibling"));
    }

    #[test]
    fn relative_path_deeper() {
        let from = Path::new("/a/b/project/crates/foo");
        let to = Path::new("/a/b/sibling");
        assert_eq!(relative_path(from, to), PathBuf::from("../../../sibling"));
    }

    #[test]
    fn relative_path_same() {
        let p = Path::new("/a/b/c");
        assert_eq!(relative_path(p, p), PathBuf::from("."));
    }

    #[test]
    #[ignore] // requires tests/fixtures/ which is not in the Nix build source
    fn find_external_path_deps_fixture() {
        let fixture_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/external-path-dep");
        let deps = find_external_path_deps(&fixture_dir).unwrap();
        assert_eq!(deps.len(), 1, "expected 1 external dep, got {:?}", deps);
        // The value should be the sanitised name
        let names: Vec<_> = deps.values().collect();
        assert!(
            names.contains(&&"external-dep-lib".to_string()),
            "expected 'external-dep-lib' in {:?}",
            names
        );
    }

    #[test]
    fn update_workspace_exclude_adds_top_level_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root_toml = dir.path().join("Cargo.toml");
        std::fs::write(
            &root_toml,
            "[workspace]\nmembers = [\"*\"]\nexclude = [\"target\"]\nresolver = \"2\"\n",
        )
        .unwrap();

        let mut mappings = HashMap::new();
        mappings.insert(PathBuf::from("/abs/my-lib"), "my-lib".to_string());
        mappings.insert(PathBuf::from("/abs/other"), "other/sub".to_string());

        update_workspace_exclude(&root_toml, &mappings).unwrap();

        let content = std::fs::read_to_string(&root_toml).unwrap();
        let doc: toml::Value = toml::from_str(&content).unwrap();
        let exclude = doc["workspace"]["exclude"].as_array().unwrap();
        let exclude_strs: Vec<&str> = exclude.iter().filter_map(|v| v.as_str()).collect();

        assert!(
            exclude_strs.contains(&"target"),
            "should preserve existing excludes: {:?}",
            exclude_strs
        );
        assert!(
            exclude_strs.contains(&"my-lib"),
            "should add top-level dir: {:?}",
            exclude_strs
        );
        // "other/sub" should be excluded as "other" (top-level component only)
        assert!(
            exclude_strs.contains(&"other"),
            "should use top-level component, not full path: {:?}",
            exclude_strs
        );
        assert!(
            !exclude_strs.contains(&"other/sub"),
            "should NOT use full sanitised path: {:?}",
            exclude_strs
        );
    }

    #[test]
    fn rewrite_cargo_tomls_with_workspace_glob() {
        // Simulates the scenario: workspace with members=["*"], an external
        // dep directory copied into the tree. Without proper exclude, cargo
        // would try to load it as a workspace member and fail.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path();
        let project_dir = dir.path(); // project_dir == dest for this test

        // Root workspace Cargo.toml with glob members
        std::fs::write(
            dest.join("Cargo.toml"),
            "[workspace]\nmembers = [\"*\"]\nexclude = [\"target\"]\nresolver = \"2\"\n",
        )
        .unwrap();

        // A member crate with an external path dep
        std::fs::create_dir_all(dest.join("app/src")).unwrap();
        std::fs::write(
            dest.join("app/Cargo.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nmy-lib = { path = \"../../my-lib\" }\n",
        )
        .unwrap();
        std::fs::write(dest.join("app/src/main.rs"), "fn main() {}\n").unwrap();

        // Simulate the external dep already copied in
        std::fs::create_dir_all(dest.join("my-lib/src")).unwrap();
        std::fs::write(
            dest.join("my-lib/Cargo.toml"),
            "[package]\nname = \"my-lib\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(dest.join("my-lib/src/lib.rs"), "").unwrap();

        // Build mappings: canonical path -> sanitised name
        let canonical = project_dir.join("../../my-lib").canonicalize().unwrap_or(
            // In test, the path doesn't exist on disk, so use the resolved path directly
            project_dir.join("../../my-lib"),
        );
        let mut mappings = HashMap::new();
        mappings.insert(canonical, "my-lib".to_string());

        rewrite_cargo_tomls(dest, project_dir, &mappings).unwrap();

        // Verify [workspace].exclude contains "my-lib"
        let root_content = std::fs::read_to_string(dest.join("Cargo.toml")).unwrap();
        let root_doc: toml::Value = toml::from_str(&root_content).unwrap();
        let exclude = root_doc["workspace"]["exclude"].as_array().unwrap();
        let exclude_strs: Vec<&str> = exclude.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            exclude_strs.contains(&"my-lib"),
            "workspace.exclude should contain 'my-lib': {:?}",
            exclude_strs
        );
    }
}
