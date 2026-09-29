//! Selective pipelining: split a library into a metadata half and a link
//! half, so that dependent libraries start on its `.rmeta` instead of
//! waiting for its codegen.
//!
//! Splitting costs a second run of the frontend and one more derivation,
//! so only libraries whose recorded codegen is long and whose frontend is a
//! small share of the compile are split. The timings come from a pipeline
//! profile that a previous build wrote with `--write-pipeline-profile`.
//! Without a profile nothing is split.

use super::{NixUnit, UnitKind};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

/// Codegen, which is the compile time after the `.rmeta` is written, below
/// which a split cannot win back the derivation and frontend it adds.
const MIN_CODEGEN_SECS: f64 = 1.0;

/// The largest share of a compile the frontend may take in a split unit.
/// The metadata half repeats the frontend, so a unit above this share
/// costs more CPU than its dependents gain in time.
const MAX_FRONTEND_SHARE: f64 = 0.6;

/// Suffix of a split library's key and derivation name that names its
/// metadata half.
const META_KEY_SUFFIX: &str = "#meta";
const META_NAME_SUFFIX: &str = "-meta";

/// A unit's part in a pipelined build.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PipelineRole {
    /// No profile was given, and the unit builds as it always has.
    #[default]
    Off,
    /// A whole rustc unit, or the link half of a split library, in a build
    /// with a profile.  `-Z no-codegen` needs `RUSTC_BOOTSTRAP=1`, which
    /// changes the crate hash, so every rustc unit of the build sets it.
    /// A crate then has one hash whether it is split or not, and a change
    /// of split decisions rebuilds no dependent.
    Whole,
    /// The metadata half of a split library.  It runs the link half's rustc
    /// invocation plus `-Z no-codegen`, and its output holds only the
    /// `.rmeta`.
    Metadata,
}

/// Recorded compile times of library units, keyed by derivation name, which
/// holds the package, its version and the crate and so stays the same
/// across machines and source edits.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PipelineProfile {
    pub units: BTreeMap<String, UnitTiming>,
}

/// One unit's compile, in seconds from the start of its build.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UnitTiming {
    /// When rustc reported the `.rmeta` written.
    pub frontend: f64,
    /// When rustc reported the `.rlib` written.
    pub total: f64,
}

impl PipelineProfile {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read pipeline profile {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse pipeline profile {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json + "\n")
            .with_context(|| format!("Failed to write pipeline profile {}", path.display()))
    }

    fn pays(&self, drv_name: &str) -> bool {
        self.units.get(drv_name).is_some_and(|t| {
            t.total - t.frontend >= MIN_CODEGEN_SECS && t.frontend <= MAX_FRONTEND_SHARE * t.total
        })
    }
}

/// Whether rustc needs only its dependencies' metadata to compile `unit`: a
/// plain library, which links nothing.
fn consumes_metadata(unit: &NixUnit) -> bool {
    unit.kind == UnitKind::Compile
        && !unit.needs_linker
        && unit
            .crate_types
            .iter()
            .all(|ct| ct == "lib" || ct == "rlib")
}

fn runs_rustc(unit: &NixUnit) -> bool {
    matches!(
        unit.kind,
        UnitKind::Compile | UnitKind::Check | UnitKind::TestCompile | UnitKind::BuildScriptCompile
    )
}

/// Splits every library that `profile` shows pays for it and that another
/// library depends on, and returns how many it split.
///
/// The link half keeps the unit's key, so linking units, which need every
/// library's object code, still reach it through their unchanged
/// dependency lists. The metadata half is added under a new key, and every
/// library, both halves of split ones included, is rewritten to compile
/// against the metadata halves of its split dependencies.
pub fn split_units(units: &mut Vec<NixUnit>, profile: &PipelineProfile) -> usize {
    let has_library_dependent: HashSet<&str> = units
        .iter()
        .filter(|u| consumes_metadata(u))
        .flat_map(|u| u.dep_extern.iter().map(|(_, k)| k.as_str()))
        .collect();
    let split: HashMap<String, String> = units
        .iter()
        .filter(|u| consumes_metadata(u) && has_library_dependent.contains(u.key.as_str()))
        .filter(|u| profile.pays(&u.drv_name))
        .map(|u| (u.key.clone(), format!("{}{META_KEY_SUFFIX}", u.key)))
        .collect();

    let halves: Vec<NixUnit> = units
        .iter()
        .filter(|u| split.contains_key(&u.key))
        .map(|u| NixUnit {
            key: split[&u.key].clone(),
            drv_name: format!("{}{META_NAME_SUFFIX}", u.drv_name),
            is_root: false,
            pipeline: PipelineRole::Metadata,
            ..u.clone()
        })
        .collect();
    for unit in units.iter_mut().filter(|u| runs_rustc(u)) {
        unit.pipeline = PipelineRole::Whole;
    }
    units.extend(halves);

    let to_meta = |key: &mut String| {
        if let Some(meta) = split.get(key.as_str()) {
            *key = meta.clone();
        }
    };
    for unit in units.iter_mut().filter(|u| consumes_metadata(u)) {
        unit.dep_extern.iter_mut().for_each(|(_, k)| to_meta(k));
        unit.all_dep_keys.iter_mut().for_each(to_meta);
    }
    split.len()
}

/// Builds a [`PipelineProfile`] from the log of a `nix build -L`, whose
/// build log lines carry the derivation name as a `<name>> ` prefix.
pub struct PipelineRecorder {
    started: HashMap<String, Instant>,
    frontend: HashMap<String, f64>,
    profile: PipelineProfile,
}

impl Default for PipelineRecorder {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineRecorder {
    pub fn new() -> Self {
        Self {
            started: HashMap::new(),
            frontend: HashMap::new(),
            profile: PipelineProfile::default(),
        }
    }

    /// Notes that Nix started building `drv_path`.
    pub fn building(&mut self, drv_path: &str, now: Instant) {
        if let Some(name) = drv_name_of(drv_path) {
            self.started.insert(name.to_string(), now);
        }
    }

    /// Reads one line of `nix build -L` output and returns whether it was a
    /// rustc artifact notification, which the caller then need not show.
    ///
    /// Nix prefixes a log line with the derivation name cut before its
    /// version, which several units share, so the unit comes from the
    /// artifact's path instead: rustc writes into `$out`, whose store path
    /// ends in the full derivation name.
    pub fn log_line(&mut self, line: &str, now: Instant) -> bool {
        let Some((_, text)) = line.split_once("> ") else {
            return false;
        };
        let Some((emit, name)) = artifact(text) else {
            return false;
        };
        let Some(&start) = self.started.get(&name) else {
            return true;
        };
        let secs = now.duration_since(start).as_secs_f64();
        match emit.as_str() {
            "metadata" => {
                self.frontend.insert(name, secs);
            }
            "link" => {
                if let Some(&frontend) = self.frontend.get(&name) {
                    let timing = UnitTiming {
                        frontend,
                        total: secs,
                    };
                    self.profile.units.insert(name, timing);
                }
            }
            _ => {}
        }
        true
    }

    pub fn finish(self) -> PipelineProfile {
        self.profile
    }
}

/// The `emit` of a rustc `--json=artifacts` notification, and the name of
/// the derivation whose output holds the artifact.
fn artifact(text: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("$message_type")?.as_str()? != "artifact" {
        return None;
    }
    let emit = value.get("emit")?.as_str()?.to_string();
    let path = value.get("artifact")?.as_str()?;
    // A library and a binary of one package share a derivation name, and
    // only libraries are ever split.
    if emit == "link" && !path.ends_with(".rlib") {
        return None;
    }
    let path = path.strip_prefix("/nix/store/")?;
    let out = path.split('/').next()?;
    Some((emit, out.split_once('-')?.1.to_string()))
}

/// `<name>` of `/nix/store/<hash>-<name>.drv`.
fn drv_name_of(drv_path: &str) -> Option<&str> {
    let base = drv_path.rsplit('/').next()?.strip_suffix(".drv")?;
    Some(base.split_once('-')?.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(key: &str, crate_type: &str, deps: &[&str]) -> NixUnit {
        let mut unit: NixUnit = serde_json::from_value(serde_json::json!({
            "key": key,
            "drv_name": format!("{key}-0.1.0-{key}"),
            "kind": "Compile",
            "source_file": "",
            "crate_name": key,
            "crate_types": [crate_type],
            "edition": "2021",
            "features": [],
            "dep_extern": [],
            "all_dep_keys": [],
            "build_script_dep": null,
            "build_script_compile_key": null,
            "manifest_dir": "",
            "cargo_envs": [],
            "extra_filename": "-0",
            "needs_linker": crate_type != "lib",
            "is_local": false,
            "links": null,
            "links_dep_keys": [],
            "profile": {"rustc_args": [], "opt_level": "3", "debug": false, "root": "release"},
            "drv_path": null,
        }))
        .unwrap();
        unit.dep_extern = deps
            .iter()
            .map(|d| (d.to_string(), d.to_string()))
            .collect();
        unit
    }

    /// `a` is a library that the library `b` depends on, and the binary `c`
    /// links both.
    fn chain() -> Vec<NixUnit> {
        let mut c = unit("c", "bin", &["b"]);
        c.all_dep_keys = vec!["a".into(), "b".into()];
        let mut b = unit("b", "lib", &["a"]);
        b.all_dep_keys = vec!["a".into()];
        vec![unit("a", "lib", &[]), b, c]
    }

    fn profile(frontend: f64, total: f64) -> PipelineProfile {
        let timing = UnitTiming { frontend, total };
        PipelineProfile {
            units: [("a-0.1.0-a".to_string(), timing)].into(),
        }
    }

    fn by_key<'a>(units: &'a [NixUnit], key: &str) -> &'a NixUnit {
        units.iter().find(|u| u.key == key).unwrap()
    }

    #[test]
    fn split_library_feeds_libraries_its_metadata_and_linkers_its_rlib() {
        let mut units = chain();
        assert_eq!(split_units(&mut units, &profile(1.0, 5.0)), 1);

        let meta = by_key(&units, "a#meta");
        assert_eq!(meta.pipeline, PipelineRole::Metadata);
        assert_eq!(meta.drv_name, "a-0.1.0-a-meta");
        assert_eq!(by_key(&units, "a").pipeline, PipelineRole::Whole);

        let b = by_key(&units, "b");
        assert_eq!(b.dep_extern, vec![("a".to_string(), "a#meta".to_string())]);
        assert_eq!(b.all_dep_keys, vec!["a#meta".to_string()]);

        let c = by_key(&units, "c");
        assert_eq!(c.dep_extern, vec![("b".to_string(), "b".to_string())]);
        assert_eq!(c.all_dep_keys, vec!["a".to_string(), "b".to_string()]);
        assert!(units.iter().all(|u| u.pipeline != PipelineRole::Off));
    }

    #[test]
    fn short_codegen_or_long_frontend_is_not_split() {
        for (frontend, total) in [(1.0, 1.5), (4.0, 5.0)] {
            let mut units = chain();
            assert_eq!(split_units(&mut units, &profile(frontend, total)), 0);
            assert_eq!(units.len(), 3);
            assert_eq!(by_key(&units, "b").dep_extern[0].1, "a");
            assert!(units.iter().all(|u| u.pipeline == PipelineRole::Whole));
        }
    }

    /// A library that only binaries depend on gains nothing, because a
    /// linking unit needs its object code either way.
    #[test]
    fn library_without_library_dependent_is_not_split() {
        let mut units = vec![unit("a", "lib", &[]), unit("c", "bin", &["a"])];
        assert_eq!(split_units(&mut units, &profile(1.0, 5.0)), 0);
    }

    #[test]
    fn recorder_times_library_artifacts_from_the_build_start() {
        let t0 = Instant::now();
        let at = |ms| t0 + std::time::Duration::from_millis(ms);
        let artifact = |name: &str, file: &str, emit: &str| {
            format!(
                "a> {{\"$message_type\":\"artifact\",\"artifact\":\"/nix/store/0000-{name}/{file}\",\"emit\":\"{emit}\"}}"
            )
        };
        let mut recorder = PipelineRecorder::new();
        recorder.building("/nix/store/hash-a-0.1.0-a.drv", at(0));
        assert!(recorder.log_line(&artifact("a-0.1.0-a", "liba.rmeta", "metadata"), at(1000)));
        assert!(!recorder.log_line("a> warning: unused", at(1500)));
        assert!(recorder.log_line(&artifact("a-0.1.0-a", "liba.rlib", "link"), at(4000)));
        recorder.building("/nix/store/hash-c-0.1.0-c.drv", at(5000));
        assert!(recorder.log_line(&artifact("c-0.1.0-c", "c.rmeta", "metadata"), at(5100)));
        assert!(!recorder.log_line(&artifact("c-0.1.0-c", "c", "link"), at(6000)));

        let profile = recorder.finish();
        assert_eq!(profile.units.len(), 1);
        let a = &profile.units["a-0.1.0-a"];
        assert!((a.frontend - 1.0).abs() < 1e-9 && (a.total - 4.0).abs() < 1e-9);
    }
}
