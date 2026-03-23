use super::util::sanitize_drv_name;
use super::{NixUnit, UnitKind};
use anyhow::Result;
use cargo::core::FeatureValue;
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

        if log::log_enabled!(log::Level::Debug) {
            let mut dep_names: Vec<&str> = dep_extern_map.keys().map(|s| s.as_str()).collect();
            dep_names.sort();
            log::debug!(
                "dep_extern_map for {} after first pass ({} entries): {:?}",
                key,
                dep_extern_map.len(),
                dep_names,
            );
        }

        // Fix missing optional deps activated by features but absent from the
        // unit graph edge list.  This happens when optional deps are declared
        // under always-false platform gates like
        // [target.'cfg(any())'.dependencies] — cargo omits the dependency edge
        // even when a feature explicitly activates it via `dep:` syntax.
        if kind != UnitKind::BuildScriptRun {
            // Build a map of feature -> [(extern_name, dep_unit_key)] from the
            // manifest's feature definitions and the full unit graph.
            let mut feature_dep_activations: HashMap<String, Vec<(String, String)>> =
                HashMap::new();
            let summary = unit.pkg.manifest().summary();
            for feat in &unit.features {
                let fvs = match summary.features().get(feat) {
                    Some(v) => v,
                    None => continue,
                };
                for fv in fvs {
                    let dep_toml_name = match fv {
                        FeatureValue::Dep { dep_name } => *dep_name,
                        FeatureValue::DepFeature {
                            dep_name,
                            weak: false,
                            ..
                        } => *dep_name,
                        _ => continue,
                    };
                    let extern_name = dep_toml_name.as_str().replace('-', "_");
                    // Look up the actual crate name (may differ with
                    // `package = "..."` in the dep spec).
                    let pkg_name = unit
                        .pkg
                        .dependencies()
                        .iter()
                        .find(|d| d.name_in_toml() == dep_toml_name)
                        .map(|d| d.package_name())
                        .unwrap_or(dep_toml_name);
                    // Find the matching lib Unit in the full graph.
                    if let Some(candidate) = all_units.iter().find(|c| {
                        c.pkg.name() == pkg_name
                            && c.target.is_lib()
                            && !c.target.is_custom_build()
                            && c.mode != CompileMode::RunCustomBuild
                    }) {
                        let dep_key = key_map[candidate].clone();
                        let already_present = dep_extern_map.contains_key(&extern_name);
                        log::debug!(
                            "Feature dep:{} for {} → extern={}, pkg={}, dep_key={}, \
                             already_in_dep_extern={}",
                            dep_toml_name,
                            key,
                            extern_name,
                            pkg_name,
                            dep_key,
                            already_present,
                        );
                        feature_dep_activations
                            .entry(feat.to_string())
                            .or_default()
                            .push((extern_name, dep_key));
                    } else {
                        log::warn!(
                            "Feature-activated dep {} (dep:{}) not found in unit graph for {}",
                            extern_name,
                            dep_toml_name,
                            key,
                        );
                    }
                }
            }

            for (extern_name, dep_key) in
                find_missing_feature_deps(&dep_extern_map, &features, &feature_dep_activations)
            {
                log::info!(
                    "Adding missing optional dep {} -> {} for {} \
                     (feature-activated, possibly behind platform gate)",
                    extern_name,
                    dep_key,
                    key,
                );
                dep_extern_map.insert(extern_name, dep_key);
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

    // Feature unification: when the same crate appears with different feature sets
    // (e.g., host vs target feature split in v2 resolver), merge them into one
    // compilation with the union of all features. Without this, transitive consumers
    // may link against different feature variants of the same crate, causing type
    // mismatches (e.g., proc_macro2::Span from variant A != proc_macro2::Span from B).
    unify_feature_variants(&mut nix_units);

    // Validate: every dep_extern key must reference an existing NixUnit.
    {
        let valid_keys: HashSet<&str> = nix_units.iter().map(|u| u.key.as_str()).collect();
        for u in &nix_units {
            for (ext_name, dep_key) in &u.dep_extern {
                if !valid_keys.contains(dep_key.as_str()) {
                    log::warn!(
                        "Stale dep_extern after unification: {} has {} -> key {} \
                         which does NOT match any NixUnit",
                        u.key,
                        ext_name,
                        dep_key,
                    );
                }
            }
        }
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

/// Return `(extern_name, dep_key)` pairs for optional deps that are activated
/// by features but missing from `dep_extern`.
///
/// This handles the case where an optional dependency is declared under an
/// always-false platform gate (e.g. `[target.'cfg(any())'.dependencies]`) and
/// Cargo omits the unit-graph edge even though a feature explicitly activates
/// the dep via `dep:` syntax.
///
/// `feature_dep_activations` maps each enabled feature name to the list of
/// `(extern_name, dep_unit_key)` pairs it can provide.  Only entries whose
/// `extern_name` is absent from `dep_extern` are returned.
fn find_missing_feature_deps(
    dep_extern: &HashMap<String, String>,
    enabled_features: &[String],
    feature_dep_activations: &HashMap<String, Vec<(String, String)>>,
) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for feat in enabled_features {
        if let Some(activations) = feature_dep_activations.get(feat) {
            for (extern_name, dep_key) in activations {
                if !dep_extern.contains_key(extern_name)
                    && !result.iter().any(|(e, _)| e == extern_name)
                {
                    result.push((extern_name.clone(), dep_key.clone()));
                }
            }
        }
    }
    result
}

/// Compute a grouping key for feature unification.  Two NixUnits with the same
/// group key differ only in their feature sets and should be merged.
fn feature_agnostic_group_key(u: &NixUnit) -> String {
    let mode_suffix = match u.kind {
        UnitKind::BuildScriptRun => "-run",
        UnitKind::BuildScriptCompile => "-build-script",
        UnitKind::TestCompile => "-test",
        UnitKind::Compile => "",
    };
    let kind_suffix = if u.for_host { "-host" } else { "" };
    let mut ct = u.crate_types.clone();
    ct.sort();
    format!(
        "{}/{}/{}{}{}/{}:{:?}",
        u.crate_name, u.edition, u.source_file, mode_suffix, kind_suffix, u.manifest_dir, ct,
    )
}

/// Merge NixUnits that differ only in features into a single unit with the
/// union of all features.  This prevents inconsistent `--extern` edges when
/// cargo's v2 resolver produces separate host/target feature sets for the same
/// crate (e.g. proc_macro2 with and without `span-locations`).
fn unify_feature_variants(nix_units: &mut Vec<NixUnit>) {
    use log::info;

    // Group indices by a feature-agnostic key.
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, u) in nix_units.iter().enumerate() {
        let gk = feature_agnostic_group_key(u);
        groups.entry(gk).or_default().push(i);
    }

    // Find groups that need merging (>1 member = same crate, different features).
    let mut redirect: HashMap<String, String> = HashMap::new(); // old_key → new_key
    let mut remove_indices: HashSet<usize> = HashSet::new();

    for indices in groups.values() {
        if indices.len() <= 1 {
            continue;
        }

        // Compute union of features across all variants.
        let mut unified_features: BTreeSet<String> = BTreeSet::new();
        for &idx in indices {
            for f in &nix_units[idx].features {
                unified_features.insert(f.clone());
            }
        }
        let unified: Vec<String> = unified_features.into_iter().collect();

        // Pick the first index as the survivor; mark the rest for removal.
        let survivor = indices[0];
        for &idx in &indices[1..] {
            redirect.insert(nix_units[idx].key.clone(), String::new()); // placeholder
            remove_indices.insert(idx);
        }

        let u = &nix_units[survivor];
        info!(
            "Feature unification: merging {} variants of {} (features: {:?} → {:?})",
            indices.len(),
            u.crate_name,
            indices
                .iter()
                .map(|&i| &nix_units[i].features)
                .collect::<Vec<_>>(),
            unified,
        );

        // Recompute key and extra_filename with unified features.
        let pkg_version = u
            .cargo_envs
            .iter()
            .find(|(k, _)| k == "CARGO_PKG_VERSION")
            .map(|(_, v)| v.as_str())
            .unwrap_or("0.0.0");
        let target_name_field = u
            .cargo_envs
            .iter()
            .find(|(k, _)| k == "CARGO_CRATE_NAME")
            .map(|(_, v)| v.as_str())
            .unwrap_or(&u.crate_name);
        let new_extra_filename = compute_extra_filename(
            &u.crate_name.replace('_', "-"), // pkg_name uses hyphens
            pkg_version,
            target_name_field,
            &unified,
            &u.crate_types,
        );

        // Compute new key from a synthetic identity string.
        let mode_suffix = match u.kind {
            UnitKind::BuildScriptRun => "-run",
            UnitKind::BuildScriptCompile => "-build-script",
            UnitKind::TestCompile => "-test",
            UnitKind::Compile => "",
        };
        let kind_suffix = if u.for_host { "-host" } else { "" };
        let mut crate_types_sorted = u.crate_types.clone();
        crate_types_sorted.sort();
        let pkg_name = u
            .cargo_envs
            .iter()
            .find(|(k, _)| k == "CARGO_PKG_NAME")
            .map(|(_, v)| v.as_str())
            .unwrap_or(&u.crate_name);
        let unified_identity = format!(
            "{}-{}-{}{}{}-{}-{:?}-{}",
            pkg_name,
            pkg_version,
            target_name_field,
            mode_suffix,
            kind_suffix,
            u.edition,
            crate_types_sorted,
            unified.join(","),
        );
        let hash = Sha256::digest(unified_identity.as_bytes());
        let short_hash = format!(
            "{:016x}",
            u64::from_le_bytes(hash[..8].try_into().expect("SHA-256 produces ≥8 bytes"))
        );
        let type_suffix = if u.crate_types.iter().any(|ct| ct == "proc-macro") {
            "-pm"
        } else {
            ""
        };
        let new_key = sanitize_drv_name(&format!(
            "{}-{}-{}{}{}-{}",
            pkg_name,
            pkg_version,
            target_name_field.replace('_', "-"),
            mode_suffix,
            type_suffix,
            short_hash,
        ));

        // Update redirect map with the actual new key.
        let old_survivor_key = nix_units[survivor].key.clone();
        redirect.insert(old_survivor_key, new_key.clone());
        for &idx in &indices[1..] {
            redirect.insert(nix_units[idx].key.clone(), new_key.clone());
        }

        // Merge is_root and target_name from all variants.
        let mut merged_is_root = false;
        let mut merged_target_name = String::new();
        for &idx in indices {
            if nix_units[idx].is_root {
                merged_is_root = true;
                if merged_target_name.is_empty() {
                    merged_target_name = nix_units[idx].target_name.clone();
                }
            }
        }

        // Merge dep_extern: union across all variants, preferring the smallest key
        // (which will be redirected later).
        let mut merged_dep_extern: HashMap<String, String> = HashMap::new();
        for &idx in indices {
            for (ext_name, dep_key) in &nix_units[idx].dep_extern {
                merged_dep_extern
                    .entry(ext_name.clone())
                    .and_modify(|existing| {
                        if *dep_key < *existing {
                            *existing = dep_key.clone();
                        }
                    })
                    .or_insert_with(|| dep_key.clone());
            }
        }
        let mut merged_dep_extern_vec: Vec<(String, String)> =
            merged_dep_extern.into_iter().collect();
        merged_dep_extern_vec.sort_by(|a, b| a.0.cmp(&b.0));

        // Apply to survivor.
        let u = &mut nix_units[survivor];
        u.key = new_key;
        u.features = unified;
        u.extra_filename = new_extra_filename;
        u.is_root = merged_is_root;
        if merged_is_root && !merged_target_name.is_empty() {
            u.target_name = merged_target_name;
        }
        u.dep_extern = merged_dep_extern_vec;
    }

    if redirect.is_empty() {
        return;
    }

    // Apply redirects to ALL units' dependency edges (including the survivor itself,
    // since its deps may reference keys that were merged).
    let apply = |key: &str| -> String {
        redirect
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string())
    };
    for u in nix_units.iter_mut() {
        for (_, dep_key) in &mut u.dep_extern {
            *dep_key = apply(dep_key);
        }
        if let Some(ref mut k) = u.build_script_dep {
            *k = apply(k);
        }
        if let Some(ref mut k) = u.build_script_compile_key {
            *k = apply(k);
        }
        for (dep_key, _) in &mut u.links_dep_keys {
            *dep_key = apply(dep_key);
        }
    }

    // Remove merged-away units (iterate in reverse to keep indices stable).
    let mut sorted_removes: Vec<usize> = remove_indices.into_iter().collect();
    sorted_removes.sort_unstable_by(|a, b| b.cmp(a));
    for idx in sorted_removes {
        nix_units.remove(idx);
    }

    // Deduplicate dep_extern entries that now point to the same key after redirects.
    for u in nix_units.iter_mut() {
        let mut seen: HashMap<String, usize> = HashMap::new();
        let mut deduped = Vec::new();
        for (ext_name, dep_key) in u.dep_extern.drain(..) {
            if seen.contains_key(&dep_key) {
                // Same dep_key for different extern names — keep both (this is valid:
                // a crate can re-export under multiple extern names).
                deduped.push((ext_name, dep_key));
            } else {
                seen.insert(dep_key.clone(), deduped.len());
                deduped.push((ext_name, dep_key));
            }
        }
        u.dep_extern = deduped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal NixUnit for testing.  Only the fields relevant to
    /// feature-unification and dependency tracking are populated.
    fn make_unit(name: &str, version: &str, features: &[&str], deps: &[(&str, &str)]) -> NixUnit {
        let feats: Vec<String> = features.iter().map(|f| f.to_string()).collect();
        let crate_types = vec!["lib".to_string()];
        let extra_filename = compute_extra_filename(name, version, name, &feats, &crate_types);
        let identity = format!(
            "{}-{}-{}-{}-{:?}-{}",
            name,
            version,
            name,
            "2021",
            crate_types,
            feats.join(","),
        );
        let hash = Sha256::digest(identity.as_bytes());
        let short_hash = format!(
            "{:016x}",
            u64::from_le_bytes(hash[..8].try_into().expect("8 bytes"))
        );
        let key = sanitize_drv_name(&format!("{}-{}-{}-{}", name, version, name, short_hash));
        NixUnit {
            key,
            drv_name: format!("{}-{}-{}", name, version, name),
            kind: UnitKind::Compile,
            source_file: format!("/nix/store/fake/{}/src/lib.rs", name),
            crate_name: name.replace('-', "_"),
            crate_types,
            edition: "2021".into(),
            features: feats,
            dep_extern: deps
                .iter()
                .map(|(ext, key)| (ext.to_string(), key.to_string()))
                .collect(),
            all_dep_keys: Vec::new(),
            build_script_dep: None,
            build_script_compile_key: None,
            manifest_dir: format!("/nix/store/fake/{}", name),
            cargo_envs: vec![
                ("CARGO_PKG_NAME".into(), name.into()),
                ("CARGO_PKG_VERSION".into(), version.into()),
                ("CARGO_CRATE_NAME".into(), name.replace('-', "_")),
            ],
            extra_filename,
            needs_linker: false,
            is_local: false,
            links: None,
            links_dep_keys: Vec::new(),
            is_root: false,
            target_name: String::new(),
            for_host: false,
            drv_path: None,
        }
    }

    // -- feature_agnostic_group_key -------------------------------------------

    #[test]
    fn group_key_same_for_different_features() {
        let a = make_unit("proc-macro2", "1.0.106", &["default"], &[]);
        let b = make_unit(
            "proc-macro2",
            "1.0.106",
            &["default", "span-locations"],
            &[],
        );
        assert_eq!(
            feature_agnostic_group_key(&a),
            feature_agnostic_group_key(&b)
        );
    }

    #[test]
    fn group_key_differs_for_different_crates() {
        let a = make_unit("proc-macro2", "1.0.106", &["default"], &[]);
        let b = make_unit("quote", "1.0.40", &["default"], &[]);
        assert_ne!(
            feature_agnostic_group_key(&a),
            feature_agnostic_group_key(&b)
        );
    }

    #[test]
    fn group_key_differs_for_host_vs_target() {
        let mut a = make_unit("proc-macro2", "1.0.106", &["default"], &[]);
        let b = make_unit("proc-macro2", "1.0.106", &["default"], &[]);
        a.for_host = true;
        assert_ne!(
            feature_agnostic_group_key(&a),
            feature_agnostic_group_key(&b)
        );
    }

    #[test]
    fn group_key_differs_for_different_kinds() {
        let mut a = make_unit("foo", "1.0.0", &[], &[]);
        let b = make_unit("foo", "1.0.0", &[], &[]);
        a.kind = UnitKind::TestCompile;
        assert_ne!(
            feature_agnostic_group_key(&a),
            feature_agnostic_group_key(&b)
        );
    }

    // -- unify_feature_variants: no-op when no duplicates ---------------------

    #[test]
    fn unify_noop_when_no_duplicates() {
        let mut units = vec![
            make_unit("serde", "1.0.0", &["default", "derive"], &[]),
            make_unit("quote", "1.0.0", &["default"], &[]),
        ];
        let keys_before: Vec<String> = units.iter().map(|u| u.key.clone()).collect();
        unify_feature_variants(&mut units);
        assert_eq!(units.len(), 2);
        let keys_after: Vec<String> = units.iter().map(|u| u.key.clone()).collect();
        assert_eq!(keys_before, keys_after);
    }

    // -- unify_feature_variants: merges feature variants ----------------------

    #[test]
    fn unify_merges_feature_variants() {
        let mut units = vec![
            make_unit("proc-macro2", "1.0.106", &["default", "proc-macro"], &[]),
            make_unit(
                "proc-macro2",
                "1.0.106",
                &["default", "proc-macro", "span-locations"],
                &[],
            ),
        ];
        unify_feature_variants(&mut units);
        assert_eq!(units.len(), 1, "should merge into one unit");
        assert_eq!(
            units[0].features,
            vec!["default", "proc-macro", "span-locations"],
            "features should be the union"
        );
    }

    #[test]
    fn unify_merges_non_subset_features() {
        // Features {A, B} and {A, C} → {A, B, C}
        let mut units = vec![
            make_unit("foo", "1.0.0", &["a", "b"], &[]),
            make_unit("foo", "1.0.0", &["a", "c"], &[]),
        ];
        unify_feature_variants(&mut units);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].features, vec!["a", "b", "c"]);
    }

    // -- unify_feature_variants: dependency edge consistency -------------------

    #[test]
    fn unify_redirects_dep_edges() {
        // Simulate the proc_macro2 scenario:
        //   pm2_v1 (key "pm2-v1") — features {default}
        //   pm2_v2 (key "pm2-v2") — features {default, span-locations}
        //   quote  depends on pm2_v1
        //   attr   depends on pm2_v2
        // After unification, both quote and attr should point to the SAME pm2 key.
        let mut pm2_v1 = make_unit("proc-macro2", "1.0.0", &["default"], &[]);
        pm2_v1.key = "pm2-v1".into();
        let mut pm2_v2 = make_unit("proc-macro2", "1.0.0", &["default", "span-locations"], &[]);
        pm2_v2.key = "pm2-v2".into();

        let mut quote = make_unit("quote", "1.0.0", &["default"], &[("proc_macro2", "pm2-v1")]);
        quote.key = "quote-key".into();

        let mut attr = make_unit(
            "proc-macro-error-attr2",
            "2.0.0",
            &["default"],
            &[("proc_macro2", "pm2-v2"), ("quote", "quote-key")],
        );
        attr.key = "attr-key".into();

        let mut units = vec![pm2_v1, pm2_v2, quote, attr];
        unify_feature_variants(&mut units);

        // pm2 variants merged into one
        let pm2_units: Vec<_> = units
            .iter()
            .filter(|u| u.crate_name == "proc_macro2")
            .collect();
        assert_eq!(pm2_units.len(), 1, "pm2 should be merged into one unit");
        let merged_pm2_key = &pm2_units[0].key;

        // Both quote and attr should now reference the same pm2 key
        let quote_unit = units.iter().find(|u| u.crate_name == "quote").unwrap();
        let attr_unit = units
            .iter()
            .find(|u| u.crate_name == "proc_macro_error_attr2")
            .unwrap();

        let quote_pm2_dep = quote_unit
            .dep_extern
            .iter()
            .find(|(name, _)| name == "proc_macro2")
            .map(|(_, k)| k.as_str());
        let attr_pm2_dep = attr_unit
            .dep_extern
            .iter()
            .find(|(name, _)| name == "proc_macro2")
            .map(|(_, k)| k.as_str());

        assert_eq!(
            quote_pm2_dep,
            Some(merged_pm2_key.as_str()),
            "quote's proc_macro2 dep should point to merged key"
        );
        assert_eq!(
            attr_pm2_dep,
            Some(merged_pm2_key.as_str()),
            "attr's proc_macro2 dep should point to merged key"
        );
        assert_eq!(
            quote_pm2_dep, attr_pm2_dep,
            "quote and attr must agree on which proc_macro2 they link"
        );
    }

    #[test]
    fn unify_preserves_root_status() {
        let mut a = make_unit("my-bin", "0.1.0", &["feat-a"], &[]);
        a.is_root = true;
        a.target_name = "my-bin".into();
        let b = make_unit("my-bin", "0.1.0", &["feat-a", "feat-b"], &[]);

        let mut units = vec![a, b];
        unify_feature_variants(&mut units);
        assert_eq!(units.len(), 1);
        assert!(units[0].is_root, "root status should be preserved");
        assert_eq!(units[0].target_name, "my-bin");
    }

    #[test]
    fn unify_does_not_merge_host_and_target() {
        let mut host = make_unit("proc-macro2", "1.0.0", &["default"], &[]);
        host.for_host = true;
        let target = make_unit("proc-macro2", "1.0.0", &["default", "span-locations"], &[]);

        let mut units = vec![host, target];
        unify_feature_variants(&mut units);
        assert_eq!(
            units.len(),
            2,
            "host and target variants must NOT be merged"
        );
    }

    #[test]
    fn unify_redirects_build_script_dep() {
        let mut bs_v1 = make_unit("proc-macro2", "1.0.0", &["default"], &[]);
        bs_v1.key = "pm2-bs-v1".into();
        bs_v1.kind = UnitKind::BuildScriptRun;
        let mut bs_v2 = make_unit("proc-macro2", "1.0.0", &["default", "span-locations"], &[]);
        bs_v2.key = "pm2-bs-v2".into();
        bs_v2.kind = UnitKind::BuildScriptRun;

        let mut consumer = make_unit("proc-macro2", "1.0.0", &["default"], &[]);
        consumer.key = "pm2-lib".into();
        consumer.build_script_dep = Some("pm2-bs-v1".into());

        let mut units = vec![bs_v1, bs_v2, consumer];
        unify_feature_variants(&mut units);

        let lib_unit = units.iter().find(|u| u.kind == UnitKind::Compile).unwrap();
        let bs_units: Vec<_> = units
            .iter()
            .filter(|u| u.kind == UnitKind::BuildScriptRun)
            .collect();
        assert_eq!(bs_units.len(), 1, "build script variants should merge");
        assert_eq!(
            lib_unit.build_script_dep.as_deref(),
            Some(bs_units[0].key.as_str()),
            "build_script_dep should be redirected to the merged key"
        );
    }

    // -- compute_extra_filename -----------------------------------------------

    #[test]
    fn extra_filename_differs_with_features() {
        let a = compute_extra_filename("foo", "1.0.0", "foo", &["a".into()], &["lib".into()]);
        let b = compute_extra_filename(
            "foo",
            "1.0.0",
            "foo",
            &["a".into(), "b".into()],
            &["lib".into()],
        );
        assert_ne!(
            a, b,
            "different features must produce different extra_filename"
        );
    }

    #[test]
    fn extra_filename_deterministic() {
        let a = compute_extra_filename("foo", "1.0.0", "foo", &["x".into()], &["lib".into()]);
        let b = compute_extra_filename("foo", "1.0.0", "foo", &["x".into()], &["lib".into()]);
        assert_eq!(a, b);
    }

    // -- find_missing_feature_deps (cfg(any()) optional dep fix) ---------------

    #[test]
    fn missing_feature_deps_adds_cfg_any_dep() {
        // Simulate indexmap: features=["serde"] activates dep:serde_core and
        // dep:serde, but cargo omits both from the unit graph because serde is
        // under [target.'cfg(any())'.dependencies].
        let mut dep_extern = HashMap::new();
        dep_extern.insert("equivalent".into(), "equivalent-key".into());
        dep_extern.insert("hashbrown".into(), "hashbrown-key".into());

        let features = vec!["default".into(), "std".into(), "serde".into()];

        let mut activations = HashMap::new();
        activations.insert(
            "serde".into(),
            vec![
                ("serde_core".into(), "serde-core-key".into()),
                ("serde".into(), "serde-key".into()),
            ],
        );

        let additions = find_missing_feature_deps(&dep_extern, &features, &activations);

        assert_eq!(
            additions.len(),
            2,
            "both serde and serde_core should be added"
        );
        assert!(
            additions
                .iter()
                .any(|(e, k)| e == "serde" && k == "serde-key"),
            "serde should be added"
        );
        assert!(
            additions
                .iter()
                .any(|(e, k)| e == "serde_core" && k == "serde-core-key"),
            "serde_core should be added"
        );
    }

    #[test]
    fn missing_feature_deps_skips_already_present() {
        // If serde is already in dep_extern (normal case without cfg(any())),
        // no additions should be made.
        let mut dep_extern = HashMap::new();
        dep_extern.insert("serde".into(), "serde-key".into());

        let features = vec!["serde".into()];
        let mut activations = HashMap::new();
        activations.insert(
            "serde".into(),
            vec![("serde".into(), "serde-different-key".into())],
        );

        let additions = find_missing_feature_deps(&dep_extern, &features, &activations);
        assert!(
            additions.is_empty(),
            "should not duplicate a dep that already exists"
        );
    }

    #[test]
    fn missing_feature_deps_noop_when_no_dep_features() {
        let dep_extern = HashMap::new();
        let features = vec!["default".into(), "std".into()];
        let activations = HashMap::new();

        let additions = find_missing_feature_deps(&dep_extern, &features, &activations);
        assert!(additions.is_empty());
    }

    #[test]
    fn missing_feature_deps_no_duplicate_across_features() {
        // Two features both activate the same dep — it should only appear once.
        let dep_extern = HashMap::new();
        let features = vec!["feat_a".into(), "feat_b".into()];
        let mut activations = HashMap::new();
        activations.insert("feat_a".into(), vec![("serde".into(), "serde-key".into())]);
        activations.insert(
            "feat_b".into(),
            vec![("serde".into(), "serde-key-alt".into())],
        );

        let additions = find_missing_feature_deps(&dep_extern, &features, &activations);
        assert_eq!(additions.len(), 1, "serde should appear only once");
        assert_eq!(additions[0].0, "serde");
    }
}
