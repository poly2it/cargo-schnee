use super::util::sanitize_drv_name;
use super::{NixUnit, UnitKind};
use anyhow::Result;
use cargo::core::compiler::{CompileKind, CompileMode, Unit};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

/// Extract NixUnit structs from the BuildContext's unit graph.
/// Returns units in topological order (dependencies before dependents).
pub(super) fn extract_units_from_bcx(
    bcx: &cargo::core::compiler::BuildContext<'_, '_>,
    roots: &[Unit],
    src: &Path,
    vendor_dir: &Path,
) -> Result<Vec<NixUnit>> {
    let src_str = src.to_string_lossy();
    let vendor_str = vendor_dir.to_string_lossy();

    // Assign keys and deduplicate units that compile identically.
    // Cargo can create multiple Unit entries for the same crate with different
    // dep_hash/profile values — they compile to the same output, so we merge them.
    let mut key_map: HashMap<Unit, String> = HashMap::new();
    let mut all_units: Vec<Unit> = bcx.unit_graph.keys().cloned().collect();
    all_units.sort_by_key(unit_sort_key);

    // Group units by their "compilation identity" (what affects rustc output).
    // Cargo can produce multiple Unit entries with the same identity but different
    // dep_hash values — we merge them and collect all dep variants for later
    // deterministic resolution.
    let mut identity_to_key: HashMap<String, String> = HashMap::new();
    let mut deduped_units: Vec<Unit> = Vec::new();
    let mut deduped_set: HashSet<String> = HashSet::new();
    // Collect ALL cargo Units per identity, so we can merge their deps later
    let mut identity_all_units: HashMap<String, Vec<Unit>> = HashMap::new();

    for unit in &all_units {
        let identity = compilation_identity(unit);
        identity_all_units
            .entry(identity.clone())
            .or_default()
            .push(unit.clone());
        if let Some(existing_key) = identity_to_key.get(&identity) {
            // Duplicate — map to the same key as the existing unit
            key_map.insert(unit.clone(), existing_key.clone());
        } else {
            let key = make_unit_key(unit);
            identity_to_key.insert(identity, key.clone());
            key_map.insert(unit.clone(), key.clone());
            if deduped_set.insert(key) {
                deduped_units.push(unit.clone());
            }
        }
    }

    // Build set of root unit keys (units the user requested to build)
    let root_keys: HashSet<String> = roots
        .iter()
        .filter_map(|u| key_map.get(u).cloned())
        .collect();
    // Map root keys to their target names
    let root_target_names: HashMap<String, String> = roots
        .iter()
        .filter_map(|u| {
            key_map
                .get(u)
                .map(|k| (k.clone(), u.target.name().to_string()))
        })
        .collect();

    // Topological sort on deduplicated units
    let topo_units = toposort(&deduped_units, &bcx.unit_graph, &key_map)?;

    let mut nix_units = Vec::new();

    for unit in &topo_units {
        let key = key_map[unit].clone();
        let identity = compilation_identity(unit);

        // Determine unit kind
        let kind = if unit.mode == CompileMode::RunCustomBuild {
            UnitKind::BuildScriptRun
        } else if unit.target.is_custom_build() {
            UnitKind::BuildScriptCompile
        } else if unit.mode == CompileMode::Test {
            UnitKind::TestCompile
        } else {
            UnitKind::Compile
        };

        // Source file
        let source_file = unit
            .target
            .src_path()
            .path()
            .map(|p| p.to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("Metabuild targets not supported: {}", key))?;

        // Map source file to nix store path
        let source_file_str = map_to_store_path(
            &source_file.to_string_lossy(),
            &src_str,
            &vendor_str,
            unit.pkg.root(),
        );

        // Crate name: rustc expects underscores
        let crate_name = unit.target.name().replace('-', "_");

        // Crate types from target kind
        let crate_types = target_kind_to_crate_types(unit);

        // Edition
        let edition = unit.target.edition().to_string();

        // Features → cfg flags (sorted for deterministic output)
        let mut features: Vec<String> = unit.features.iter().map(|f| f.to_string()).collect();
        features.sort();

        // Dependencies: merge from ALL cargo Units with the same identity.
        // Cargo may create multiple Units for the same crate (different dep_hash)
        // that reference different variants of their deps (e.g., libc with different
        // features). Since the compilation output is identical regardless of which
        // variant is linked, we canonicalize by picking the lexicographically smallest
        // dep key for each slot. This ensures deterministic derivation JSON across runs.
        let all_variants = identity_all_units
            .get(&identity)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        let mut dep_extern_map: HashMap<String, String> = HashMap::new();
        let mut build_script_dep: Option<String> = None;
        let mut build_script_compile_key: Option<String> = None;
        let mut links_dep_map: HashMap<String, String> = HashMap::new();

        for variant_unit in all_variants {
            let deps = bcx
                .unit_graph
                .get(variant_unit)
                .cloned()
                .unwrap_or_default();
            for dep in &deps {
                let dep_key = &key_map[&dep.unit];
                if dep.unit.mode == CompileMode::RunCustomBuild {
                    if kind == UnitKind::BuildScriptRun {
                        if let Some(links_name) = dep.unit.pkg.manifest().links() {
                            links_dep_map
                                .entry(links_name.to_string())
                                .and_modify(|existing| {
                                    if *dep_key < *existing {
                                        *existing = dep_key.clone();
                                    }
                                })
                                .or_insert_with(|| dep_key.clone());
                        }
                    } else {
                        build_script_dep = Some(match build_script_dep {
                            Some(ref existing) if *dep_key < *existing => dep_key.clone(),
                            Some(ref existing) => existing.clone(),
                            None => dep_key.clone(),
                        });
                    }
                } else if dep.unit.target.is_custom_build() {
                    build_script_compile_key = Some(match build_script_compile_key {
                        Some(ref existing) if *dep_key < *existing => dep_key.clone(),
                        Some(ref existing) => existing.clone(),
                        None => dep_key.clone(),
                    });
                } else {
                    let extern_name = dep.extern_crate_name.to_string();
                    dep_extern_map
                        .entry(extern_name)
                        .and_modify(|existing| {
                            if *dep_key < *existing {
                                *existing = dep_key.clone();
                            }
                        })
                        .or_insert_with(|| dep_key.clone());
                }
            }
        }

        let mut dep_extern: Vec<(String, String)> = dep_extern_map.into_iter().collect();
        let mut links_dep_keys: Vec<(String, String)> = links_dep_map
            .into_iter()
            .map(|(links_name, dep_key)| (dep_key, links_name))
            .collect();

        // Sort dependency lists for deterministic derivation JSON ordering.
        // cargo's unit graph may iterate HashMap entries in non-deterministic order.
        dep_extern.sort_by(|a, b| a.0.cmp(&b.0));
        links_dep_keys.sort_by(|a, b| a.1.cmp(&b.1));

        // Extra-filename hash
        let extra_filename = compute_extra_filename(
            &unit.pkg.name(),
            &unit.pkg.version().to_string(),
            unit.target.name(),
            &features,
            &crate_types,
        );

        // Manifest dir — map to store path
        let manifest_dir = map_to_store_path(
            &unit.pkg.root().to_string_lossy(),
            &src_str,
            &vendor_str,
            unit.pkg.root(),
        );

        // Standard cargo env vars
        let cargo_envs = compute_cargo_envs(unit);

        let needs_linker = kind == UnitKind::TestCompile
            || crate_types
                .iter()
                .any(|ct| ct == "proc-macro" || ct == "bin" || ct == "cdylib" || ct == "dylib");

        let is_root = root_keys.contains(&key);
        let target_name = root_target_names.get(&key).cloned().unwrap_or_default();
        let for_host = matches!(unit.kind, CompileKind::Host);

        nix_units.push(NixUnit {
            key,
            drv_name: sanitize_drv_name(&format!(
                "{}-{}-{}{}",
                unit.pkg.name(),
                unit.pkg.version(),
                unit.target.name(),
                mode_suffix_for_drv_name(&kind),
            )),
            kind,
            source_file: source_file_str,
            crate_name,
            crate_types,
            edition,
            features,
            dep_extern,
            all_dep_keys: Vec::new(), // computed below
            build_script_dep,
            build_script_compile_key,
            manifest_dir,
            cargo_envs,
            extra_filename,
            needs_linker,
            is_local: unit.pkg.package_id().source_id().is_path(),
            links: unit.pkg.manifest().links().map(|s| s.to_string()),
            links_dep_keys,
            is_root,
            target_name,
            for_host,
            drv_path: None,
        });
    }

    // Compute transitive dependency closure (only for Compile units, for -L paths)
    let key_to_idx: HashMap<String, usize> = nix_units
        .iter()
        .enumerate()
        .map(|(i, u)| (u.key.clone(), i))
        .collect();

    for i in 0..nix_units.len() {
        if nix_units[i].kind == UnitKind::BuildScriptRun {
            continue;
        }
        let mut all_deps = HashSet::new();
        let mut stack: Vec<String> = nix_units[i]
            .dep_extern
            .iter()
            .map(|(_, k)| k.clone())
            .collect();
        while let Some(dep_key) = stack.pop() {
            if !all_deps.insert(dep_key.clone()) {
                continue;
            }
            if let Some(&idx) = key_to_idx.get(&dep_key) {
                for (_, transitive_key) in &nix_units[idx].dep_extern {
                    stack.push(transitive_key.clone());
                }
            }
        }
        let mut all: Vec<String> = all_deps.into_iter().collect();
        all.sort();
        nix_units[i].all_dep_keys = all;
    }

    Ok(nix_units)
}

pub(super) fn mode_suffix_for_drv_name(kind: &UnitKind) -> &'static str {
    match kind {
        UnitKind::BuildScriptCompile => "-build-script",
        UnitKind::BuildScriptRun => "-run-build-script",
        UnitKind::TestCompile => "-test",
        UnitKind::Compile => "",
    }
}

/// Compute a "compilation identity" for a unit — units with the same identity
/// produce identical rustc output and can be deduplicated.
fn compilation_identity(unit: &Unit) -> String {
    let mode_suffix = match unit.mode {
        CompileMode::RunCustomBuild => "-run",
        CompileMode::Test => "-test",
        _ if unit.target.is_custom_build() => "-build-script",
        _ => "",
    };
    // For cross-compilation, the same crate may appear as both Host and Target —
    // these are distinct compilations and must not be deduplicated.
    let kind_suffix = match unit.kind {
        CompileKind::Host => "-host",
        CompileKind::Target(_) => "",
    };
    let mut crate_types: Vec<String> = unit
        .target
        .rustc_crate_types()
        .into_iter()
        .map(|ct| ct.as_str().to_string())
        .collect();
    crate_types.sort();
    let mut feats: Vec<&str> = unit.features.iter().map(|f| f.as_str()).collect();
    feats.sort();
    format!(
        "{}-{}-{}{}{}-{}-{:?}-{}",
        unit.pkg.name(),
        unit.pkg.version(),
        unit.target.name(),
        mode_suffix,
        kind_suffix,
        unit.target.edition(),
        crate_types,
        feats.join(","),
    )
}

/// Create a stable sort key for a unit (must be deterministic across runs).
fn unit_sort_key(unit: &Unit) -> String {
    compilation_identity(unit)
}

/// Generate a deterministic unique key for a unit.
/// Uses SHA-256 of the compilation identity to guarantee stability across runs
/// (DefaultHasher uses randomized SipHash seeds, breaking nix derivation caching).
fn make_unit_key(unit: &Unit) -> String {
    let identity = compilation_identity(unit);
    let hash = Sha256::digest(identity.as_bytes());
    let short_hash = format!(
        "{:016x}",
        u64::from_le_bytes(hash[..8].try_into().expect("SHA-256 produces ≥8 bytes"))
    );

    let mode_suffix = match unit.mode {
        CompileMode::RunCustomBuild => "-run",
        CompileMode::Test => "-test",
        _ if unit.target.is_custom_build() => "-build-script",
        _ => "",
    };
    let crate_types = target_kind_to_crate_types(unit);
    let type_suffix = if crate_types.iter().any(|ct| ct == "proc-macro") {
        "-pm"
    } else {
        ""
    };
    sanitize_drv_name(&format!(
        "{}-{}-{}{}{}-{}",
        unit.pkg.name(),
        unit.pkg.version(),
        unit.target.name(),
        mode_suffix,
        type_suffix,
        short_hash,
    ))
}

/// Convert a target kind to the rustc --crate-type strings.
pub(super) fn target_kind_to_crate_types(unit: &Unit) -> Vec<String> {
    unit.target
        .rustc_crate_types()
        .into_iter()
        .map(|ct| ct.as_str().to_string())
        .collect()
}

/// Map a filesystem path to a nix store path reference.
pub(super) fn map_to_store_path(
    path: &str,
    src_str: &str,
    vendor_str: &str,
    pkg_root: &Path,
) -> String {
    // If it's under src (project source), keep as-is (already a store path)
    if path.starts_with(src_str) {
        return path.to_string();
    }
    // If it's under vendor dir, keep as-is
    if path.starts_with(vendor_str) {
        return path.to_string();
    }
    // Try to map via package root (for vendored crates)
    let pkg_root_str = pkg_root.to_string_lossy();
    if path.starts_with(pkg_root_str.as_ref()) {
        return path.to_string();
    }
    path.to_string()
}

/// Compute deterministic extra-filename hash for a unit.
/// Includes features and crate types to avoid StableCrateId collisions
/// when the same crate is compiled with different configurations.
pub(super) fn compute_extra_filename(
    pkg_name: &str,
    pkg_version: &str,
    target_name: &str,
    features: &[String],
    crate_types: &[String],
) -> String {
    let input = format!(
        "{}-{}-{}-{}-{}",
        pkg_name,
        pkg_version,
        target_name,
        features.join(","),
        crate_types.join(",")
    );
    let hash = Sha256::digest(input.as_bytes());
    format!(
        "-{:016x}",
        u64::from_le_bytes(hash[..8].try_into().expect("SHA-256 produces ≥8 bytes"))
    )
}

/// Compute standard CARGO_PKG_* env vars from unit info.
pub(super) fn compute_cargo_envs(unit: &Unit) -> Vec<(String, String)> {
    let pkg = &unit.pkg;
    let manifest = pkg.manifest();
    let metadata = manifest.metadata();
    let mut envs = vec![
        ("CARGO_PKG_NAME".into(), pkg.name().to_string()),
        ("CARGO_PKG_VERSION".into(), pkg.version().to_string()),
        (
            "CARGO_PKG_VERSION_MAJOR".into(),
            pkg.version().major.to_string(),
        ),
        (
            "CARGO_PKG_VERSION_MINOR".into(),
            pkg.version().minor.to_string(),
        ),
        (
            "CARGO_PKG_VERSION_PATCH".into(),
            pkg.version().patch.to_string(),
        ),
        (
            "CARGO_PKG_VERSION_PRE".into(),
            pkg.version().pre.to_string(),
        ),
        (
            "CARGO_CRATE_NAME".into(),
            unit.target.name().replace('-', "_"),
        ),
    ];
    let authors = metadata.authors.join(":");
    envs.push(("CARGO_PKG_AUTHORS".into(), authors));
    if let Some(ref desc) = metadata.description {
        envs.push(("CARGO_PKG_DESCRIPTION".into(), desc.clone()));
    }
    if let Some(ref homepage) = metadata.homepage {
        envs.push(("CARGO_PKG_HOMEPAGE".into(), homepage.clone()));
    }
    if let Some(ref repository) = metadata.repository {
        envs.push(("CARGO_PKG_REPOSITORY".into(), repository.clone()));
    }
    if let Some(ref license) = metadata.license {
        envs.push(("CARGO_PKG_LICENSE".into(), license.clone()));
    }
    if let Some(ref license_file) = metadata.license_file {
        envs.push(("CARGO_PKG_LICENSE_FILE".into(), license_file.clone()));
    }
    if let Some(ref rust_version) = metadata.rust_version {
        envs.push(("CARGO_PKG_RUST_VERSION".into(), rust_version.to_string()));
    }
    if let Some(ref readme) = metadata.readme {
        envs.push(("CARGO_PKG_README".into(), readme.clone()));
    }
    envs
}

/// Topological sort of units based on the unit graph.
pub(super) fn toposort(
    units: &[Unit],
    unit_graph: &HashMap<Unit, Vec<cargo::core::compiler::unit_graph::UnitDep>>,
    key_map: &HashMap<Unit, String>,
) -> Result<Vec<Unit>> {
    let mut in_degree: HashMap<String, usize> = HashMap::new();
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    let key_to_unit: HashMap<String, Unit> = units
        .iter()
        .map(|u| (key_map[u].clone(), u.clone()))
        .collect();

    for unit in units {
        let key = &key_map[unit];
        in_degree.entry(key.clone()).or_insert(0);
        adj.entry(key.clone()).or_default();
    }

    for unit in units {
        let key = &key_map[unit];
        if let Some(deps) = unit_graph.get(unit) {
            for dep in deps {
                if let Some(dep_key) = key_map.get(&dep.unit) {
                    adj.entry(dep_key.clone()).or_default().push(key.clone());
                    *in_degree.entry(key.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    let mut queue: BTreeSet<String> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(k, _)| k.clone())
        .collect();

    let mut result = Vec::new();
    while let Some(key) = queue.pop_first() {
        if let Some(unit) = key_to_unit.get(&key) {
            result.push(unit.clone());
        }
        if let Some(dependents) = adj.get(&key) {
            for dep_key in dependents {
                if let Some(deg) = in_degree.get_mut(dep_key) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.insert(dep_key.clone());
                    }
                }
            }
        }
    }

    if result.len() != units.len() {
        let sorted_keys: HashSet<String> = result.iter().map(|u| key_map[u].clone()).collect();
        let stuck: Vec<_> = units
            .iter()
            .filter(|u| !sorted_keys.contains(&key_map[u]))
            .map(|u| {
                let key = &key_map[u];
                let deps_str: String = unit_graph
                    .get(u)
                    .map(|deps| {
                        deps.iter()
                            .filter_map(|d| key_map.get(&d.unit))
                            .map(|k| {
                                let status = if sorted_keys.contains(k) {
                                    "ok"
                                } else {
                                    "STUCK"
                                };
                                format!("{}({})", k, status)
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                format!(
                    "  {} [in_degree={}] -> [{}]",
                    key,
                    in_degree.get(key).unwrap_or(&0),
                    deps_str
                )
            })
            .collect();
        anyhow::bail!(
            "Topological sort failed: cycle detected ({} of {} units sorted)\nStuck units:\n{}",
            result.len(),
            units.len(),
            stuck.join("\n")
        );
    }

    Ok(result)
}
