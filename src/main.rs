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
    /// Type-check the project without producing binaries
    Check {
        /// Path to Cargo.toml
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Use a pre-vendored dependency directory (nix store path)
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
        /// Package(s) to check (can be specified multiple times)
        #[arg(short, long)]
        package: Vec<String>,
        /// Space or comma separated list of features to activate
        #[arg(long)]
        features: Vec<String>,
        /// Do not activate the `default` feature
        #[arg(long)]
        no_default_features: bool,
    },
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
    /// Run clippy lints on the project (not yet implemented)
    Clippy,
    /// Build documentation (not yet implemented)
    Doc,
    /// Automatically apply lint suggestions (not yet implemented)
    Fix,
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

/// Resolve the workspace root directory from a manifest path.
/// If the manifest is a workspace member, walks up to find the workspace root.
/// Falls back to the manifest's parent directory if no workspace root is found.
fn resolve_workspace_root(manifest_path: &Path) -> Result<PathBuf> {
    let gctx = GlobalContext::default()?;
    match Workspace::new(manifest_path, &gctx) {
        Ok(ws) => Ok(ws.root().to_path_buf()),
        Err(_) => manifest_path
            .parent()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("Cannot determine project directory")),
    }
}

/// Find `Cargo.lock` by checking `project_dir` first, then walking up to
/// the workspace root. In workspace members the lockfile lives at the root.
fn find_lockfile(project_dir: &Path) -> Result<PathBuf> {
    let local = project_dir.join("Cargo.lock");
    if local.exists() {
        return Ok(local);
    }
    // Walk up looking for Cargo.lock next to a workspace-root Cargo.toml
    let mut dir = project_dir.to_path_buf();
    while let Some(parent) = dir.parent() {
        dir = parent.to_path_buf();
        let candidate = dir.join("Cargo.lock");
        let manifest = dir.join("Cargo.toml");
        if candidate.exists() && manifest.exists() {
            return Ok(candidate);
        }
    }
    // Fall back to the original path so the existing error message is preserved
    Ok(local)
}

/// Read [package].name from a Cargo.toml.
fn read_package_name(manifest_path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
    let doc: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", manifest_path.display()))?;
    doc.get("package")
        .and_then(|p| p.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("No [package].name found in {}", manifest_path.display()))
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

/// Strip leading `../` components from a relative path to produce an in-tree name.
/// Mirrors `sanitiseName` in `buildPackage.nix`.
/// e.g. `../my-lib` → `my-lib`, `../../foo/bar` → `foo/bar`
///
/// Uses proper path component parsing rather than naive string replacement so
/// that paths like `../foo/../bar` are handled correctly (→ `foo/../bar`; only
/// leading `..` components are stripped).
fn sanitise_extra_source_name(rel_path: &str) -> Result<String> {
    use std::path::Component;
    let path = Path::new(rel_path);
    let components: Vec<_> = path.components().collect();
    let leading_parent_count = components
        .iter()
        .take_while(|c| matches!(c, Component::ParentDir))
        .count();
    if leading_parent_count == 0 {
        anyhow::bail!(
            "Extra source path must start with '../' components: {}",
            rel_path
        );
    }
    let remaining: PathBuf = components[leading_parent_count..].iter().collect();
    let result = remaining.to_string_lossy().to_string();
    if result.is_empty() {
        anyhow::bail!(
            "Extra source path has no name after stripping '../': {}",
            rel_path
        );
    }
    Ok(result)
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
///
/// When a dep is a sub-crate inside an external workspace (i.e. the sub-crate
/// inherits workspace properties), the entire workspace root is added to the
/// map so that workspace inheritance still resolves in the store tree.
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
            // Check if this dep is inside an external workspace.
            // If so, copy the entire workspace root to preserve inheritance.
            if let Some(ws_root) = find_enclosing_workspace_root(&canonical, project_canonical) {
                let ws_rel = relative_path(project_dir, &ws_root);
                let ws_rel_str = ws_rel.to_string_lossy();
                if let Ok(ws_sanitised) = sanitise_extra_source_name(&ws_rel_str) {
                    // Map the workspace root for copying
                    external
                        .entry(ws_root.clone())
                        .or_insert(ws_sanitised.clone());
                    // Map the dep path for rewriting (sub-path within the workspace)
                    if canonical != ws_root {
                        let sub_path = canonical.strip_prefix(&ws_root).unwrap_or(Path::new(""));
                        let dep_sanitised =
                            format!("{}/{}", ws_sanitised, sub_path.to_string_lossy());
                        external.entry(canonical).or_insert(dep_sanitised);
                    }
                }
            } else {
                let rel = relative_path(project_dir, &canonical);
                let rel_str = rel.to_string_lossy();
                if let Ok(sanitised) = sanitise_extra_source_name(&rel_str) {
                    external.entry(canonical).or_insert(sanitised);
                }
            }
        }
    }
}

/// Walk up from `path` looking for an enclosing workspace root (a directory
/// with a `Cargo.toml` containing a `[workspace]` section).  Stops before
/// reaching `stop_at` (the project root) to avoid matching the project itself.
/// Returns the workspace root directory if found.
fn find_enclosing_workspace_root(path: &Path, stop_at: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()?.to_path_buf()
    };
    let stop_canonical = stop_at
        .canonicalize()
        .unwrap_or_else(|_| stop_at.to_path_buf());
    let mut candidate = dir.parent()?.to_path_buf();
    loop {
        // Don't match the project directory itself or anything above it.
        // Also stop when the candidate is an ancestor of stop_at — any
        // workspace there would encompass the project itself.
        let candidate_canonical = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        if candidate_canonical == stop_canonical
            || stop_canonical.starts_with(&candidate_canonical)
            || !candidate_canonical.starts_with("/")
        {
            break;
        }
        let manifest = candidate.join("Cargo.toml");
        if manifest.exists()
            && let Ok(content) = std::fs::read_to_string(&manifest)
            && let Ok(doc) = toml::from_str::<toml::Value>(&content)
            && doc.get("workspace").is_some()
        {
            return Some(candidate);
        }
        candidate = match candidate.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
    }
    None
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
    let mut allowed_files = collect_git_files(project_dir)?;

    // Include extra gitignored files specified in [*.metadata.schnee.extra-includes]
    let extra_patterns = read_extra_includes(&project_dir.join("Cargo.toml"));
    let mut extra_outside: Vec<(PathBuf, PathBuf)> = Vec::new(); // (abs_path, store_rel_path)
    if !extra_patterns.is_empty() {
        let canon_proj = project_dir.canonicalize().ok();
        let mut count = 0usize;
        for pattern in &extra_patterns {
            // glob crate: `dir/**` only matches the dir itself (zero components).
            // Normalise to `dir/**/*` so files are matched recursively.
            let pat = if pattern.ends_with("**") {
                format!("{}/*", pattern)
            } else {
                pattern.clone()
            };
            let full = project_dir.join(&pat).to_string_lossy().to_string();
            match glob::glob(&full) {
                Ok(paths) => {
                    for entry in paths.flatten() {
                        if !entry.is_file() {
                            continue;
                        }
                        // Canonicalize the entry so that glob results containing
                        // ".." are resolved before the strip_prefix check.
                        // Without this, strip_prefix succeeds for paths like
                        // "<project_dir>/../sibling/file" and classifies them
                        // as inside the project (with a "../..." relative path
                        // that escapes the store tree when joined with dest).
                        let canon_entry = match entry.canonicalize() {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        let Some(ref canon_proj) = canon_proj else {
                            continue;
                        };
                        if let Ok(rel) = canon_entry.strip_prefix(canon_proj) {
                            // Inside project_dir — add to allowed files
                            if let Some(ref mut files) = allowed_files {
                                files.insert(rel.to_path_buf());
                            }
                            count += 1;
                        } else {
                            // Outside project_dir — compute _parent-based relative path
                            // Walk up from project_dir to find common ancestor, replacing
                            // each '..' with '_parent'.
                            let proj_comps: Vec<_> = canon_proj.components().collect();
                            let entry_comps: Vec<_> = canon_entry.components().collect();
                            let common = proj_comps
                                .iter()
                                .zip(entry_comps.iter())
                                .take_while(|(a, b)| a == b)
                                .count();
                            let mut store_rel = PathBuf::new();
                            for _ in common..proj_comps.len() {
                                store_rel.push("_parent");
                            }
                            for comp in &entry_comps[common..] {
                                store_rel.push(comp);
                            }
                            extra_outside.push((canon_entry, store_rel));
                            count += 1;
                        }
                    }
                }
                Err(e) => log::warn!("Invalid extra-includes pattern '{}': {}", pattern, e),
            }
        }
        if count > 0 {
            shell::status("Including", &format!("{} extra source files", count));
        }
    }

    // Detect external path dependencies
    let external_deps = find_external_path_deps(project_dir).unwrap_or_else(|e| {
        log::warn!("Failed to detect external path deps: {}", e);
        HashMap::new()
    });

    // Fast path: no external deps or outside extra includes → try in-process NAR cache
    if external_deps.is_empty()
        && extra_outside.is_empty()
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

    // Copy extra-includes that live outside the project directory
    for (abs_path, store_rel) in &extra_outside {
        let dest_path = dest.join(store_rel);
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(abs_path, &dest_path)
            .with_context(|| format!("Failed to copy extra include {}", abs_path.display()))?;
    }

    // Copy external path deps into the store tree and rewrite Cargo.toml paths
    if !external_deps.is_empty() {
        // Collect all source roots so we can skip sub-paths already covered
        // by a workspace root copy (e.g. don't copy sub-crate/ separately
        // when its parent workspace/ is already being copied).
        let roots: Vec<&PathBuf> = external_deps.keys().collect();
        for (abs_path, sanitised) in &external_deps {
            let dominated = roots
                .iter()
                .any(|r| *r != abs_path && abs_path.starts_with(r));
            if dominated {
                continue;
            }
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

/// Read extra-includes glob patterns from `[workspace.metadata.schnee]` or
/// `[package.metadata.schnee]` in the given Cargo.toml.
fn read_extra_includes(manifest_path: &Path) -> Vec<String> {
    let content = match std::fs::read_to_string(manifest_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let doc: toml::Value = match toml::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = doc
        .get("workspace")
        .and_then(|w| w.get("metadata"))
        .and_then(|m| m.get("schnee"))
        .and_then(|s| s.get("extra-includes"))
        .and_then(|v| v.as_array())
        .or_else(|| {
            doc.get("package")
                .and_then(|p| p.get("metadata"))
                .and_then(|m| m.get("schnee"))
                .and_then(|s| s.get("extra-includes"))
                .and_then(|v| v.as_array())
        });
    match arr {
        Some(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => Vec::new(),
    }
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

/// A single unit-graph cache entry, keyed by
/// `hash(Cargo.lock + Cargo.toml + profile + target + intent + packages + features)`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
struct UnitGraphCacheEntry {
    src_store: String,
    units: Vec<plan_nix::NixUnit>,
    target_cfg_envs: Vec<(String, String)>,
    host_cfg_envs: Vec<(String, String)>,
}

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
    /// Unit graph cache, keyed by a composite of Cargo.lock, workspace manifests,
    /// profile, target, intent, packages, and features.  Multiple entries coexist
    /// so that e.g. `--package X` does not evict the full-workspace entry.
    #[serde(default)]
    unit_graphs: HashMap<String, UnitGraphCacheEntry>,
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
                    shell::DrvKind::Check => " (check)",
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

/// Look up `CARGO_TARGET_{TRIPLE}_RUNNER` env var (e.g. `wine64` for Windows targets).
/// Returns `(program, prefix_args)` if set, or `None` for direct execution.
fn resolve_runner(target: &Option<String>) -> Option<(String, Vec<String>)> {
    let triple = target.as_ref()?;
    let env_key = format!(
        "CARGO_TARGET_{}_RUNNER",
        triple.to_uppercase().replace('-', "_")
    );
    let runner = std::env::var(&env_key).ok()?;
    let parts: Vec<String> = runner.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        return None;
    }
    Some((parts[0].clone(), parts[1..].to_vec()))
}

/// Check whether `--target` specifies a cross-compilation target (differs from host).
fn is_cross_target(target: &Option<String>) -> bool {
    let Some(triple) = target.as_ref() else {
        return false;
    };
    let host = format!("{}-unknown-linux-gnu", std::env::consts::ARCH);
    triple != &host
}

/// Compute the binary filename for a given target name, appending `.exe` for Windows targets.
fn binary_name(target_name: &str, target: &Option<String>) -> String {
    let is_windows = target
        .as_ref()
        .map(|t| t.contains("windows"))
        .unwrap_or(false);
    if is_windows {
        format!("{target_name}.exe")
    } else {
        target_name.to_string()
    }
}

/// Execute a binary, optionally through a runner (e.g. Wine for cross-compiled Windows binaries).
/// Errors with a helpful message if cross-compiling without a runner set.
/// When using a runner, its stderr is captured and only shown if the process fails.
fn run_binary(
    binary_path: &Path,
    args: &[String],
    target: &Option<String>,
    manifest_dir: Option<&str>,
) -> Result<std::process::ExitStatus> {
    let runner = resolve_runner(target);
    if runner.is_none() && is_cross_target(target) {
        let triple = target.as_ref().unwrap();
        let env_key = format!(
            "CARGO_TARGET_{}_RUNNER",
            triple.to_uppercase().replace('-', "_")
        );
        anyhow::bail!(
            "cannot execute cross-compiled binary for target `{triple}`\n\
             Set {env_key} to a runner (e.g. `wine64` for Windows targets)"
        );
    }
    let mut cmd = if let Some((ref prog, ref prefix_args)) = runner {
        let mut cmd = Command::new(prog);
        cmd.args(prefix_args).arg(binary_path).args(args);
        // Suppress Wine debug noise (fixme, err, etc.) unless the user
        // has explicitly configured WINEDEBUG.
        if std::env::var_os("WINEDEBUG").is_none() {
            cmd.env("WINEDEBUG", "-all");
        }
        cmd
    } else {
        let mut cmd = Command::new(binary_path);
        cmd.args(args);
        cmd
    };
    // Set CARGO_MANIFEST_DIR so that runtime lookups via std::env::var()
    // resolve to the writable project directory instead of the nix store.
    // Also set the working directory to match vanilla cargo behavior.
    if let Some(dir) = manifest_dir {
        cmd.env("CARGO_MANIFEST_DIR", dir);
        cmd.current_dir(dir);
    }
    let status = cmd
        .status()
        .with_context(|| format!("Failed to execute {}", binary_path.display()))?;
    Ok(status)
}

/// Create a deterministic `/tmp` symlink for CARGO_MANIFEST_DIR.
///
/// At compile time the derivation creates the same symlink pointing to the
/// Nix store path so proc macros can read files. Here at runtime we
/// re-create it pointing to the writable project directory, so both
/// `env!("CARGO_MANIFEST_DIR")` (baked at compile time) and
/// `std::env::var("CARGO_MANIFEST_DIR")` resolve to a readable+writable
/// location.
fn schnee_manifest_symlink(project_manifest_dir: &str) -> String {
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(project_manifest_dir.as_bytes());
        nix_encoding::hex_lower(&hasher.finalize()[..8])
    };
    let tmp_path = format!("/tmp/_schnee_md_{}", hash);
    // Atomically replace any stale symlink (may point to a store path from
    // a previous build).
    let _ = std::fs::remove_file(&tmp_path);
    let _ = std::os::unix::fs::symlink(project_manifest_dir, &tmp_path);
    tmp_path
}

struct BuildResult {
    /// Root derivation paths with target names, unit kinds, manifest dirs, and binary paths
    root_drvs: Vec<(String, String, plan_nix::UnitKind, String, Option<PathBuf>)>,
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
    let project_dir_buf = resolve_workspace_root(&manifest_path)?;
    let project_dir = project_dir_buf.as_path();

    // When the user's manifest points to a workspace member (not the root)
    // and no -p packages were given, scope the build to that member — matching
    // standard `cargo` behaviour of respecting the working directory.
    let ws_root_manifest = project_dir.join("Cargo.toml");
    let ws_root_manifest_canon = ws_root_manifest.canonicalize().unwrap_or(ws_root_manifest);
    let packages = if packages.is_empty() && manifest_path != ws_root_manifest_canon {
        let pkg_name = read_package_name(&manifest_path)?;
        log::info!(
            "Scoping build to package '{}' (manifest at {})",
            pkg_name,
            manifest_path.display(),
        );
        vec![pkg_name]
    } else {
        packages.to_vec()
    };
    let packages = &packages[..];

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
    let lockfile_path = find_lockfile(project_dir)?;
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
        UserIntent::Check { .. } => "check",
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
    let cached_entry = cache.unit_graphs.get(&unit_graph_key);
    let cached_units = cached_entry.map(|e| {
        log::info!("Unit graph cache hit ({} units)", e.units.len());
        (e.src_store.clone(), e.units.clone())
    });
    let cached_cfg_envs = cached_entry.map(|e| e.target_cfg_envs.clone());
    let cached_host_cfg_envs = cached_entry.map(|e| e.host_cfg_envs.clone());

    // Resolve passthrough env vars for build-script derivations.
    // CARGO_SCHNEE_PASSTHRU_ENVS is a space-separated list of env var names
    // whose values should be forwarded into per-crate build-script derivations.
    let passthru_envs: Vec<(String, String)> = std::env::var("CARGO_SCHNEE_PASSTHRU_ENVS")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|name| std::env::var(name).ok().map(|val| (name.to_string(), val)))
        .collect();

    let (root_drvs, plan_units, cfg_envs, host_cfg_envs) = plan_nix::run_plan_nix(
        Path::new(&src_store),
        Path::new(&vendor_store),
        verify_drv_paths,
        &mut cache.tool_closures,
        cached_units,
        cached_cfg_envs,
        cached_host_cfg_envs,
        &profile,
        &target_config,
        user_intent,
        packages,
        features,
        no_default_features,
        &passthru_envs,
        Some(project_dir),
    )?;

    // Update unit graph cache
    cache.unit_graphs.insert(
        unit_graph_key,
        UnitGraphCacheEntry {
            src_store: src_store.clone(),
            units: plan_units
                .iter()
                .map(|u| {
                    let mut c = u.clone();
                    c.clear_drv_path();
                    c
                })
                .collect(),
            target_cfg_envs: cfg_envs,
            host_cfg_envs,
        },
    );

    let plan_duration = plan_start.elapsed();

    // Build all root derivations
    let project_pkg_name = read_bin_target_name(&manifest_path).ok();
    let mut cmd = Command::new("nix-store");
    cmd.arg("--realise");
    for (drv_path, _, _) in &root_drvs {
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
    let mut nix_error_lines: Vec<String> = Vec::new();

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
                shell::DrvKind::Check => {
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
                        shell::status("Checking", &msg);
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
        } else if trimmed.starts_with("resolved derivation:")
            || trimmed.starts_with("warning: you did not specify")
            || trimmed.starts_with("copying path '")
            || (trimmed.starts_with("/nix/store/") && !trimmed.contains("failed"))
        {
        } else if !trimmed.is_empty() {
            progress.clear();
            // Nix prefixes "Last N log lines" with "> "; strip before checking
            let content = trimmed.strip_prefix("> ").unwrap_or(trimmed);
            let was_diagnostic = diagnostics::emit_line(
                &mut diag_shell,
                content,
                &src_store_prefix,
                &project_dir_prefix,
            );
            if !was_diagnostic {
                nix_error_lines.push(trimmed.to_string());
            }
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
        // Extract failed .drv paths from nix output so we can suggest `nix log`.
        let failed_drvs: Vec<&str> = nix_error_lines
            .iter()
            .filter_map(|line| {
                // nix outputs: builder for '/nix/store/xxx.drv' failed with exit code N
                let rest = line.strip_prefix("builder for '")?;
                let drv = rest.split('\'').next()?;
                if drv.ends_with(".drv") {
                    Some(drv)
                } else {
                    None
                }
            })
            .collect();

        // Check for ca-derivations errors specifically.
        let has_ca_error = nix_error_lines.iter().any(|l| l.contains("ca-derivations"));

        if has_ca_error {
            anyhow::bail!(
                "Ensure your nix.conf includes:\n  \
                 experimental-features = nix-command flakes ca-derivations\n\
                 Or set NIX_CONFIG=\"extra-experimental-features = ca-derivations\"."
            );
        }
        for line in &nix_error_lines {
            eprintln!("{}", line);
        }
        if !failed_drvs.is_empty() {
            eprintln!("For derivation logs, run:");
            for drv in &failed_drvs {
                eprintln!("  nix log {}", drv);
            }
        }
        std::process::exit(1);
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

    // Track actual binary paths per root for test/bench runners
    let mut root_bin_paths: Vec<Option<PathBuf>> = Vec::with_capacity(root_drvs.len());

    for (idx, (_, target_name, kind)) in root_drvs.iter().enumerate() {
        let out_path = out_paths.get(idx).ok_or_else(|| {
            anyhow::anyhow!(
                "Missing output path for root {} (got {} paths for {} roots)",
                target_name,
                out_paths.len(),
                root_drvs.len()
            )
        })?;

        let mut bin_file = None;
        let is_windows_target = target_config.is_windows();
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
            let fname = entry.file_name().to_string_lossy().to_string();
            if is_windows_target {
                // Windows PE binaries don't have Unix execute bits.
                // Detect executables by .exe extension; also recognise .dll
                // (cdylib crates) so they get a clean-named link too.
                if fname.ends_with(".exe") || fname.ends_with(".dll") {
                    bin_file = Some(dest);
                }
            } else {
                use std::os::unix::fs::PermissionsExt;
                let src_mode = std::fs::metadata(entry.path())?.permissions().mode();
                let is_exec = src_mode & 0o111 != 0;
                let new_mode = if is_exec { 0o755 } else { 0o644 };
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(new_mode))?;
                if is_exec {
                    bin_file = Some(dest);
                }
            }
        }
        // Only create clean-named links for non-test roots, matching cargo's
        // behavior where test binaries live in deps/ and never overwrite the
        // main binary in target/<profile>/.
        let is_test_root = matches!(kind, plan_nix::UnitKind::TestCompile);
        if !is_test_root {
            if let Some(ref bin_path) = bin_file {
                // For Windows targets, use the correct extension (.exe or .dll)
                let bin_ext = bin_path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let clean_name = if is_windows_target && !bin_ext.is_empty() {
                    format!("{}.{}", target_name, bin_ext)
                } else {
                    target_name.to_string()
                };
                let clean_dest = target_debug.join(&clean_name);
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
        root_bin_paths.push(bin_file);
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

    // Build root_drvs with manifest dirs and binary paths
    let root_drvs_with_kind: Vec<(String, String, plan_nix::UnitKind, String, Option<PathBuf>)> =
        root_drvs
            .into_iter()
            .enumerate()
            .map(|(idx, (drv_path, target_name, kind))| {
                let unit = plan_units
                    .iter()
                    .find(|u| u.drv_path.as_deref() == Some(&drv_path));
                // Map store-path manifest_dir back to the project directory for
                // runtime CARGO_MANIFEST_DIR (covers std::env::var() lookups).
                let manifest_dir = unit
                    .map(|u| {
                        let store_prefix = src_store_prefix.trim_end_matches('/');
                        if let Some(suffix) = u.manifest_dir.strip_prefix(store_prefix) {
                            format!("{}{}", project_dir.display(), suffix)
                        } else {
                            u.manifest_dir.clone()
                        }
                    })
                    .unwrap_or_default();
                let bin_path = root_bin_paths.get(idx).cloned().flatten();
                (drv_path, target_name, kind, manifest_dir, bin_path)
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
        SchneeCommand::Check {
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
                UserIntent::Check { test: false },
                verify_drv_paths,
                verbose,
                &write_profile_to,
                &interrupted,
            )?;
        }
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
                .filter(|(_, _, kind, _, _)| matches!(kind, plan_nix::UnitKind::Compile))
                .collect();

            let (_, target_name, _, manifest_dir, _) = if let Some(bin_name) = bin {
                bin_roots
                    .iter()
                    .find(|(_, name, _, _, _)| name == bin_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no bin target named `{}`\navailable targets: {}",
                            bin_name,
                            bin_roots
                                .iter()
                                .map(|(_, n, _, _, _)| n.as_str())
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
                        .map(|(_, n, _, _, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            };

            let bin_name = binary_name(target_name, target);
            let binary_path = result.target_debug.join(&bin_name);
            shell::status("Running", &format!("`{}`", binary_path.display()));
            let status = run_binary(&binary_path, args, target, Some(manifest_dir))?;
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
                .filter(|(_, _, kind, _, _)| matches!(kind, plan_nix::UnitKind::TestCompile))
                .collect();

            if test_roots.is_empty() {
                shell::status("Finished", "no test targets to run");
                return Ok(());
            }

            let mut any_failed = false;
            for (_, _target_name, _, manifest_dir, bin_path) in &test_roots {
                let symlink_path = schnee_manifest_symlink(manifest_dir);
                let binary_path = bin_path
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("test root has no binary output"))?;
                shell::status("Running", &format!("tests in `{}`", binary_path.display()));
                let status = run_binary(binary_path, args, target, Some(&symlink_path))?;
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
                .filter(|(_, _, kind, _, _)| matches!(kind, plan_nix::UnitKind::TestCompile))
                .collect();

            if bench_roots.is_empty() {
                shell::status("Finished", "no bench targets to run");
                return Ok(());
            }

            let mut any_failed = false;
            for (_, _target_name, _, manifest_dir, bin_path) in &bench_roots {
                let symlink_path = schnee_manifest_symlink(manifest_dir);
                let binary_path = bin_path
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("bench root has no binary output"))?;
                shell::status(
                    "Running",
                    &format!("benchmarks in `{}`", binary_path.display()),
                );
                let mut bench_args = vec!["--bench".to_string()];
                bench_args.extend(args.iter().cloned());
                let status = run_binary(binary_path, &bench_args, target, Some(&symlink_path))?;
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
            let (_, plan_units, _, _) = plan_nix::run_plan_nix(
                Path::new(&src_store),
                Path::new(&vendor_store),
                verify_drv_paths,
                &mut closure_cache,
                None,
                None,
                None,
                &profile_cfg,
                &target_config,
                UserIntent::Build,
                package,
                features,
                no_default_features,
                &[],
                Some(project_dir),
            )?;

            println!("{}", plan::format_mermaid_graph(&plan_units));
        }
        SchneeCommand::Clippy => {
            anyhow::bail!(
                "cargo schnee clippy is not yet implemented (needs clippy-driver as rustc wrapper)"
            );
        }
        SchneeCommand::Doc => {
            anyhow::bail!("cargo schnee doc is not yet implemented (needs rustdoc integration)");
        }
        SchneeCommand::Fix => {
            anyhow::bail!("cargo schnee fix is not yet implemented (needs rustfix integration)");
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
            let (root_drvs, _, _, _) = plan_nix::run_plan_nix(
                src,
                vendor_dir,
                verify_drv_paths,
                &mut closure_cache,
                None,
                None,
                None,
                &default_profile,
                &default_target,
                UserIntent::Build,
                &[],
                &[],
                false,
                &[],
                None,
            )?;
            // Output the root .drv paths
            for (drv_path, _, _) in &root_drvs {
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
    fn sanitise_handles_intermediate_dot_dot() {
        // Only leading ../ components are stripped; intermediate ones are preserved
        assert_eq!(
            sanitise_extra_source_name("../foo/../bar").unwrap(),
            "foo/../bar"
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
