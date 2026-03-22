//! Build plan extraction: use cargo as a library to get the full unit graph.

use anyhow::{Context, Result};
use cargo::core::Workspace;
use cargo::core::compiler::UnitInterner;
use cargo::ops::{self, CompileOptions};
use cargo::util::context::GlobalContext;
use std::collections::HashMap;
use std::path::Path;

/// A serializable representation of a compilation unit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnitInfo {
    pub id: String,
    pub package_name: String,
    pub package_version: String,
    pub target_name: String,
    pub target_kind: String,
    pub compile_mode: String,
    pub features: Vec<String>,
    pub is_local: bool,
    /// Dependencies (by unit id)
    pub deps: Vec<DepInfo>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DepInfo {
    pub unit_id: String,
    pub extern_crate_name: String,
}

/// The full build plan: a DAG of compilation units.
#[derive(Debug, serde::Serialize)]
pub struct BuildPlan {
    pub units: Vec<UnitInfo>,
    pub roots: Vec<String>,
}

/// Extract the build plan from a Cargo workspace without actually building.
pub fn extract_build_plan(manifest_path: &Path) -> Result<BuildPlan> {
    let mut gctx = GlobalContext::default().context("Failed to create GlobalContext")?;
    gctx.configure(
        0,     // verbose
        false, // quiet
        None,  // color
        false, // frozen
        false, // locked
        false, // offline
        &None, // target_dir
        &[],   // unstable_flags
        &[],   // cli_config
    )
    .context("Failed to configure GlobalContext")?;

    let ws = Workspace::new(manifest_path, &gctx).context("Failed to open workspace")?;

    let mut options = CompileOptions::new(&gctx, cargo::util::command_prelude::UserIntent::Build)
        .context("Failed to create CompileOptions")?;
    if ws.is_virtual() {
        options.spec = ops::Packages::All(Vec::new());
    }

    let interner = UnitInterner::new();
    let bcx =
        ops::create_bcx(&ws, &options, &interner, None).context("Failed to create BuildContext")?;

    // Build a map from Unit -> unique ID
    let mut unit_ids: HashMap<cargo::core::compiler::Unit, String> = HashMap::new();
    // First pass: assign IDs
    for (counter, unit) in bcx.unit_graph.keys().enumerate() {
        let id = format!(
            "{}-{}-{}-{}",
            unit.pkg.name(),
            unit.pkg.version(),
            unit.target.name(),
            counter,
        );
        unit_ids.insert(unit.clone(), id);
    }

    // Second pass: build UnitInfo structs
    let mut units = Vec::new();
    for (unit, deps) in &bcx.unit_graph {
        let unit_id = unit_ids[unit].clone();
        let is_local = unit.pkg.package_id().source_id().is_path();

        let dep_infos: Vec<DepInfo> = deps
            .iter()
            .map(|dep| DepInfo {
                unit_id: unit_ids[&dep.unit].clone(),
                extern_crate_name: dep.extern_crate_name.to_string(),
            })
            .collect();

        units.push(UnitInfo {
            id: unit_id,
            package_name: unit.pkg.name().to_string(),
            package_version: unit.pkg.version().to_string(),
            target_name: unit.target.name().to_string(),
            target_kind: format!("{:?}", unit.target.kind()),
            compile_mode: format!("{:?}", unit.mode),
            features: unit.features.iter().map(|f| f.to_string()).collect(),
            is_local,
            deps: dep_infos,
        });
    }

    let roots: Vec<String> = bcx.roots.iter().map(|u| unit_ids[u].clone()).collect();

    Ok(BuildPlan { units, roots })
}

/// Format the Nix derivation graph as a Mermaid flowchart.
pub fn format_mermaid_graph(units: &[crate::plan_nix::NixUnit]) -> String {
    use crate::plan_nix::UnitKind;
    use std::collections::HashMap;
    use std::fmt::Write;

    // Build key → index map for stable node IDs
    let key_to_idx: HashMap<&str, usize> = units
        .iter()
        .enumerate()
        .map(|(i, u)| (u.key.as_str(), i))
        .collect();

    let mut out = String::from("graph LR\n");

    // Emit nodes
    for (i, unit) in units.iter().enumerate() {
        // For build scripts, extract the package name from drv_name
        // (drv_name is like "serde-1.0.219-build-script-build-build-script")
        let pkg_label = unit
            .drv_name
            .split('-')
            .take_while(|s| s.chars().next().is_none_or(|c| !c.is_ascii_digit()))
            .collect::<Vec<_>>()
            .join("-");
        let pkg_label = if pkg_label.is_empty() {
            &unit.crate_name
        } else {
            &pkg_label
        };

        let label = match unit.kind {
            UnitKind::BuildScriptCompile => format!("build({}) [compile]", pkg_label),
            UnitKind::BuildScriptRun => format!("build({}) [run]", pkg_label),
            _ => {
                let kind = if unit.crate_types.iter().any(|ct| ct == "proc-macro") {
                    "proc-macro"
                } else if unit.crate_types.iter().any(|ct| ct == "bin") {
                    "bin"
                } else if unit
                    .crate_types
                    .iter()
                    .any(|ct| ct == "dylib" || ct == "cdylib")
                {
                    "dylib"
                } else {
                    "lib"
                };
                format!("{} [{}]", unit.crate_name, kind)
            }
        };

        // Node shape by kind:
        //   {{"text"}}   hexagon   — build scripts
        //   [["text"]]   subroutine — proc-macros
        //   (["text"])   stadium   — root binaries
        //   ["text"]     rectangle — libraries
        let node = match unit.kind {
            UnitKind::BuildScriptCompile | UnitKind::BuildScriptRun => {
                format!("    n{}{{{{\"{}\"}}}}", i, label)
            }
            _ if unit.crate_types.iter().any(|ct| ct == "proc-macro") => {
                format!("    n{}[[\"{}\"]]", i, label)
            }
            _ if unit.is_root => format!("    n{}([\"{}\"])", i, label),
            _ => format!("    n{}[\"{}\"]", i, label),
        };

        let class = if unit.is_root {
            ":::root"
        } else if matches!(
            unit.kind,
            UnitKind::BuildScriptCompile | UnitKind::BuildScriptRun
        ) {
            ":::build"
        } else if unit.crate_types.iter().any(|ct| ct == "proc-macro") {
            ":::pmacro"
        } else if unit.is_local {
            ":::local"
        } else {
            ":::dep"
        };

        let _ = writeln!(out, "{}{}", node, class);
    }
    out.push('\n');

    // Emit edges
    for (i, unit) in units.iter().enumerate() {
        for (_, dep_key) in &unit.dep_extern {
            if let Some(&j) = key_to_idx.get(dep_key.as_str()) {
                let _ = writeln!(out, "    n{} --> n{}", i, j);
            }
        }
        if let Some(ref bs_key) = unit.build_script_dep
            && let Some(&j) = key_to_idx.get(bs_key.as_str())
        {
            let _ = writeln!(out, "    n{} -.-> n{}", i, j);
        }
        if let Some(ref bsc_key) = unit.build_script_compile_key
            && let Some(&j) = key_to_idx.get(bsc_key.as_str())
        {
            let _ = writeln!(out, "    n{} --> n{}", i, j);
        }
    }

    // Style classes
    out.push('\n');
    out.push_str("    classDef root fill:#f6d365,stroke:#f39c12,color:#000\n");
    out.push_str("    classDef local fill:#a8e6cf,stroke:#2ecc71,color:#000\n");
    out.push_str("    classDef dep fill:#74b9ff,stroke:#0984e3,color:#000\n");
    out.push_str("    classDef pmacro fill:#a29bfe,stroke:#6c5ce7,color:#000\n");
    out.push_str("    classDef build fill:#dfe6e9,stroke:#636e72,color:#000\n");

    out
}

/// Pretty-print the build plan as a DAG.
pub fn print_build_plan(plan: &BuildPlan) {
    eprintln!("\n=== Build Plan: {} units ===\n", plan.units.len());

    for unit in &plan.units {
        let marker = if plan.roots.contains(&unit.id) {
            " [ROOT]"
        } else {
            ""
        };
        let local = if unit.is_local { " (local)" } else { "" };

        eprintln!(
            "  {} v{} (target: {}, kind: {}, mode: {}){}{}",
            unit.package_name,
            unit.package_version,
            unit.target_name,
            unit.target_kind,
            unit.compile_mode,
            local,
            marker,
        );

        if !unit.features.is_empty() {
            eprintln!("    features: {}", unit.features.join(", "));
        }

        for dep in &unit.deps {
            eprintln!("    -> {} (as {})", dep.unit_id, dep.extern_crate_name);
        }
    }

    eprintln!();
    // Also emit JSON to stdout for machine consumption
    println!(
        "{}",
        serde_json::to_string_pretty(&plan).expect("Failed to serialize build plan")
    );
}
