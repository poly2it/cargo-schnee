//! Selective pipelining: split a library into a metadata half and a link
//! half, so that dependent libraries start on its `.rmeta` instead of
//! waiting for its codegen.
//!
//! Splitting costs a second run of the frontend and one more derivation,
//! and it shortens the build only where the library's codegen holds up the
//! longest chain of units.  So cargo-schnee models the build from a
//! pipeline profile that a previous build wrote with
//! `--write-pipeline-profile`, and splits the libraries on the modelled
//! critical path.  Without a profile nothing is split.

use super::{NixUnit, UnitKind};
use crate::nix_log::{self, Event};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;

/// The largest share of a compile the frontend may take in a split unit.
/// The metadata half repeats the frontend, so a unit above this share
/// costs more CPU than its dependents gain in time.
const MAX_FRONTEND_SHARE: f64 = 0.6;

/// Time from a unit's last input finishing until its own build starts.
/// On rust-analyzer's critical path, Nix's scheduling latency and the gap
/// between builds add up to about this much per derivation.
const HOP_SECS: f64 = 0.15;

/// The least a split must shorten the modelled build to be kept.
const MIN_GAIN_SECS: f64 = 0.1;

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

/// Recorded build times of every unit, keyed by derivation name, which
/// holds the package, its version and the crate and so stays the same
/// across machines and source edits.  The dependency structure is not
/// recorded, because the plan that applies the profile has the current one.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PipelineProfile {
    /// Units that wrote an `.rlib`.  A package's library and binary share
    /// a derivation name, so libraries are kept apart from other units.
    pub libraries: BTreeMap<String, UnitTiming>,
    /// Every other unit, which only adds its time to the chains through it.
    pub others: BTreeMap<String, UnitTiming>,
}

/// One unit's build, in seconds from the start of its derivation's build.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UnitTiming {
    /// When Nix finished the derivation.
    pub wall: f64,
    /// The CPU time the builder used, where Nix reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<f64>,
    /// When rustc reported the `.rmeta` written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontend: Option<f64>,
}

impl UnitTiming {
    /// The unit's cost in the model.  CPU time changes less with the
    /// host's load than wall time, so it is preferred where known.  But
    /// parallel codegen spends several CPU seconds per second, `hir-ty`
    /// 110 s in 24 s, so the cost never exceeds the wall time.
    fn secs(&self) -> f64 {
        self.cpu.map_or(self.wall, |cpu| cpu.min(self.wall))
    }

    /// The frontend's share of the build, measured in wall time, because
    /// rustc reports the `.rmeta` only as a point in time.
    fn frontend_share(&self) -> Option<f64> {
        self.frontend
            .filter(|_| self.wall > 0.0)
            .map(|f| f / self.wall)
    }
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

    fn timing(&self, unit: &NixUnit) -> Option<&UnitTiming> {
        let writes_rlib = unit.kind == UnitKind::Compile
            && unit
                .crate_types
                .iter()
                .any(|ct| ct == "lib" || ct == "rlib");
        let map = if writes_rlib {
            &self.libraries
        } else {
            &self.others
        };
        map.get(&unit.drv_name)
    }

    fn record(&mut self, name: String, timing: UnitTiming, rlib: bool, fresh: bool) {
        let map = if rlib {
            &mut self.libraries
        } else {
            &mut self.others
        };
        // A unit built in this run replaces the profile it was planned with.
        // Units built for the host and for the target share a name, and of
        // those the longer one is kept.
        if fresh || map.get(&name).is_none_or(|old| old.secs() < timing.secs()) {
            map.insert(name, timing);
        }
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

/// The unit graph with profiled times, on which a schedule is modelled as
/// if every unit started as soon as its inputs were done.
struct Model {
    deps: Vec<Vec<usize>>,
    /// Indices in an order where every unit follows its dependencies.
    order: Vec<usize>,
    secs: Vec<f64>,
    /// The metadata half's time, for units that may be split.
    frontend: Vec<Option<f64>>,
    consumes_metadata: Vec<bool>,
}

struct Schedule {
    finish: Vec<f64>,
    /// The dependency whose output the unit waited for last.
    last_input: Vec<Option<usize>>,
}

impl Schedule {
    fn makespan(&self) -> f64 {
        self.finish.iter().copied().fold(0.0, f64::max)
    }
}

impl Model {
    fn new(units: &[NixUnit], profile: &PipelineProfile) -> Self {
        let index: HashMap<&str, usize> = units
            .iter()
            .enumerate()
            .map(|(i, u)| (u.key.as_str(), i))
            .collect();
        let deps: Vec<Vec<usize>> = units
            .iter()
            .map(|u| {
                u.dep_extern
                    .iter()
                    .map(|(_, k)| k)
                    .chain(&u.all_dep_keys)
                    .chain(&u.build_script_dep)
                    .chain(&u.build_script_compile_key)
                    .chain(u.links_dep_keys.iter().map(|(k, _)| k))
                    .filter_map(|k| index.get(k.as_str()).copied())
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect()
            })
            .collect();
        let has_library_dependent: HashSet<usize> = units
            .iter()
            .zip(&deps)
            .filter(|(u, _)| consumes_metadata(u))
            .flat_map(|(_, d)| d.iter().copied())
            .collect();
        let timings: Vec<Option<&UnitTiming>> = units.iter().map(|u| profile.timing(u)).collect();
        let frontend = units
            .iter()
            .enumerate()
            .map(|(i, u)| {
                let timing = timings[i]?;
                let share = timing.frontend_share()?;
                (consumes_metadata(u)
                    && has_library_dependent.contains(&i)
                    && share <= MAX_FRONTEND_SHARE)
                    .then(|| share * timing.secs())
            })
            .collect();
        Self {
            order: topological_order(&deps),
            secs: timings
                .iter()
                .map(|t| t.map_or(0.0, UnitTiming::secs))
                .collect(),
            frontend,
            consumes_metadata: units.iter().map(consumes_metadata).collect(),
            deps,
        }
    }

    fn schedule(&self, split: &[bool]) -> Schedule {
        let n = self.deps.len();
        let mut finish = vec![0.0; n];
        let mut metadata = vec![0.0; n];
        let mut last_input = vec![None; n];
        for &i in &self.order {
            let ready = |d: usize| -> f64 {
                if split[d] && self.consumes_metadata[i] {
                    metadata[d]
                } else {
                    finish[d]
                }
            };
            let last = self.deps[i]
                .iter()
                .copied()
                .max_by(|&a, &b| ready(a).total_cmp(&ready(b)).then(b.cmp(&a)));
            let start = last.map_or(0.0, ready) + HOP_SECS;
            last_input[i] = last;
            finish[i] = start + self.secs[i];
            metadata[i] = start + self.frontend[i].unwrap_or(self.secs[i]);
        }
        Schedule { finish, last_input }
    }

    /// The edges of the longest chain, each as its dependency and the unit
    /// that waited for it.
    fn critical_edges(&self, schedule: &Schedule) -> Vec<(usize, usize)> {
        let Some(mut unit) =
            (0..self.deps.len()).max_by(|&a, &b| schedule.finish[a].total_cmp(&schedule.finish[b]))
        else {
            return Vec::new();
        };
        let mut edges = Vec::new();
        while let Some(dep) = schedule.last_input[unit] {
            edges.push((dep, unit));
            unit = dep;
        }
        edges
    }

    /// Splits every library whose codegen holds up a library after it on
    /// the critical path, until no such library is left, and then undoes
    /// each split whose removal lengthens the modelled build by less than
    /// [`MIN_GAIN_SECS`].
    fn select(&self) -> Vec<bool> {
        let mut split = vec![false; self.deps.len()];
        loop {
            let schedule = self.schedule(&split);
            let on_path: Vec<usize> = self
                .critical_edges(&schedule)
                .into_iter()
                .filter(|&(dep, unit)| {
                    !split[dep] && self.frontend[dep].is_some() && self.consumes_metadata[unit]
                })
                .map(|(dep, _)| dep)
                .collect();
            if on_path.is_empty() {
                break;
            }
            on_path.into_iter().for_each(|i| split[i] = true);
        }
        for i in 0..split.len() {
            if !split[i] {
                continue;
            }
            let with = self.schedule(&split).makespan();
            split[i] = false;
            let without = self.schedule(&split).makespan();
            split[i] = without - with >= MIN_GAIN_SECS;
        }
        split
    }
}

/// Kahn's algorithm over `deps`.  A unit on a cycle, which a valid plan
/// never has, is appended at the end.
fn topological_order(deps: &[Vec<usize>]) -> Vec<usize> {
    let mut pending: Vec<usize> = deps.iter().map(Vec::len).collect();
    let mut dependents = vec![Vec::new(); deps.len()];
    for (unit, unit_deps) in deps.iter().enumerate() {
        unit_deps.iter().for_each(|&d| dependents[d].push(unit));
    }
    let mut ready: VecDeque<usize> = (0..deps.len()).filter(|&i| pending[i] == 0).collect();
    let mut order = Vec::with_capacity(deps.len());
    while let Some(unit) = ready.pop_front() {
        order.push(unit);
        for &d in &dependents[unit] {
            pending[d] -= 1;
            if pending[d] == 0 {
                ready.push_back(d);
            }
        }
    }
    let placed: HashSet<usize> = order.iter().copied().collect();
    order.extend((0..deps.len()).filter(|i| !placed.contains(i)));
    order
}

/// Splits the libraries on the critical path that the build modelled from
/// `profile` shows, and returns how many it split.
///
/// The link half keeps the unit's key, so linking units, which need every
/// library's object code, still reach it through their unchanged
/// dependency lists. The metadata half is added under a new key, and every
/// library, both halves of split ones included, is rewritten to compile
/// against the metadata halves of its split dependencies.
pub fn split_units(units: &mut Vec<NixUnit>, profile: &PipelineProfile) -> usize {
    let model = Model::new(units, profile);
    let selected = model.select();
    tracing::info!(
        unsplit = model.schedule(&vec![false; units.len()]).makespan(),
        split = model.schedule(&selected).makespan(),
        "Modelled build time in seconds"
    );
    let split: HashMap<String, String> = units
        .iter()
        .zip(&selected)
        .filter(|(_, s)| **s)
        .inspect(|(u, _)| tracing::debug!(library = %u.drv_name, "Splitting"))
        .map(|(u, _)| (u.key.clone(), format!("{}{META_KEY_SUFFIX}", u.key)))
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

/// Builds a [`PipelineProfile`] from the `--log-format internal-json`
/// output of `nix build`, which ties each log line and resource report to
/// its build by activity id.
///
/// A split build records its units under the contention the split causes,
/// which moves the critical path, so a profile is meant to be re-recorded
/// from the builds it plans.  The recorder therefore starts from the
/// profile the build was planned with and replaces the units this build
/// ran, and a unit that came from the cache keeps its earlier times.
pub struct PipelineRecorder {
    clock: Instant,
    builds: HashMap<u64, Build>,
    profile: PipelineProfile,
    /// Names recorded in this run, each with whether it wrote an `.rlib`.
    recorded: HashSet<(String, bool)>,
}

/// A build in progress, with times in seconds on the log's clock.
struct Build {
    name: String,
    started: f64,
    frontend: Option<f64>,
    rlib: bool,
    cpu: Option<f64>,
}

impl PipelineRecorder {
    /// Starts from `planned_with`, the profile the build was planned with.
    pub fn new(clock: Instant, planned_with: PipelineProfile) -> Self {
        Self {
            clock,
            builds: HashMap::new(),
            profile: planned_with,
            recorded: HashSet::new(),
        }
    }

    /// Reads one line of the JSON log and returns the lines that the
    /// plain-text log would have shown for it.
    ///
    /// Nix stamps each event with `ts` when it has the profiling patch, and
    /// otherwise the time the line was read stands in.
    pub fn read(&mut self, line: &str) -> Vec<String> {
        let now = self.clock.elapsed().as_secs_f64();
        match nix_log::parse(line) {
            Event::Text(lines) => lines,
            Event::BuildStarted {
                id,
                drv_path,
                text,
                at,
            } => {
                // A metadata half repeats its library's frontend, whose time
                // the link half records.
                if let Some(name) =
                    drv_name_of(&drv_path).filter(|name| !name.ends_with(META_NAME_SUFFIX))
                {
                    let build = Build {
                        name: name.to_string(),
                        started: at.unwrap_or(now),
                        frontend: None,
                        rlib: false,
                        cpu: None,
                    };
                    self.builds.insert(id, build);
                }
                vec![text]
            }
            Event::BuildLog { id, line, at } => {
                if let Some(build) = self.builds.get_mut(&id)
                    && let Some((emit, path)) = artifact(&line)
                {
                    match emit.as_str() {
                        "metadata" => build.frontend = Some(at.unwrap_or(now) - build.started),
                        "link" if path.ends_with(".rlib") => build.rlib = true,
                        _ => {}
                    }
                }
                Vec::new()
            }
            Event::BuildResources { id, cpu_secs } => {
                if let Some(build) = self.builds.get_mut(&id) {
                    build.cpu = Some(cpu_secs);
                }
                Vec::new()
            }
            Event::Stopped { id, at } => {
                if let Some(build) = self.builds.remove(&id) {
                    let timing = UnitTiming {
                        wall: at.unwrap_or(now) - build.started,
                        cpu: build.cpu,
                        frontend: build.frontend,
                    };
                    let fresh = self.recorded.insert((build.name.clone(), build.rlib));
                    self.profile.record(build.name, timing, build.rlib, fresh);
                }
                Vec::new()
            }
            Event::Ignored => Vec::new(),
        }
    }

    pub fn finish(self) -> PipelineProfile {
        self.profile
    }
}

/// The `emit` and path of a rustc `--json=artifacts` notification.
fn artifact(text: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("$message_type")?.as_str()? != "artifact" {
        return None;
    }
    let emit = value.get("emit")?.as_str()?.to_string();
    let path = value.get("artifact")?.as_str()?.to_string();
    Some((emit, path))
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
        unit.all_dep_keys = unit.dep_extern.iter().map(|(_, k)| k.clone()).collect();
        unit
    }

    /// `a` is a library that the library `b` depends on, and the binary `c`
    /// links both.
    fn chain() -> Vec<NixUnit> {
        let mut c = unit("c", "bin", &["b"]);
        c.all_dep_keys = vec!["a".into(), "b".into()];
        vec![unit("a", "lib", &[]), unit("b", "lib", &["a"]), c]
    }

    fn timing(frontend: f64, wall: f64) -> UnitTiming {
        UnitTiming {
            wall,
            cpu: None,
            frontend: Some(frontend),
        }
    }

    fn profile(libraries: &[(&str, UnitTiming)]) -> PipelineProfile {
        PipelineProfile {
            libraries: libraries
                .iter()
                .map(|(k, t)| (format!("{k}-0.1.0-{k}"), t.clone()))
                .collect(),
            others: BTreeMap::new(),
        }
    }

    fn by_key<'a>(units: &'a [NixUnit], key: &str) -> &'a NixUnit {
        units.iter().find(|u| u.key == key).unwrap()
    }

    #[test]
    fn split_library_feeds_libraries_its_metadata_and_linkers_its_rlib() {
        let mut units = chain();
        let profile = profile(&[("a", timing(1.0, 5.0)), ("b", timing(1.0, 2.0))]);
        assert_eq!(split_units(&mut units, &profile), 1);

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
    fn long_frontend_is_not_split() {
        let mut units = chain();
        let profile = profile(&[("a", timing(4.0, 5.0)), ("b", timing(1.0, 2.0))]);
        assert_eq!(split_units(&mut units, &profile), 0);
        assert_eq!(units.len(), 3);
        assert_eq!(by_key(&units, "b").dep_extern[0].1, "a");
        assert!(units.iter().all(|u| u.pipeline == PipelineRole::Whole));
    }

    /// `tt` and `cfg` took about 2 s on rust-analyzer's critical path, but
    /// a profile from a quieter build recorded them below 1 s of codegen.
    /// Size does not matter on the critical path, only the share.
    #[test]
    fn short_library_on_the_critical_path_is_split() {
        let mut units = chain();
        let profile = profile(&[("a", timing(0.1, 0.6)), ("b", timing(1.0, 2.0))]);
        assert_eq!(split_units(&mut units, &profile), 1);
        assert_eq!(by_key(&units, "b").dep_extern[0].1, "a#meta");
    }

    /// `x` feeds the library `y`, but the binary `c` waits for `a`'s link
    /// half long after `y` is done, so splitting `x` gains nothing.
    #[test]
    fn library_off_the_critical_path_is_not_split() {
        let mut c = unit("c", "bin", &["b", "y"]);
        c.all_dep_keys = ["a", "b", "x", "y"].map(String::from).to_vec();
        let mut units = vec![
            unit("a", "lib", &[]),
            unit("b", "lib", &["a"]),
            unit("x", "lib", &[]),
            unit("y", "lib", &["x"]),
            c,
        ];
        let profile = profile(&[
            ("a", timing(1.0, 5.0)),
            ("b", timing(0.5, 1.0)),
            ("x", timing(0.2, 2.0)),
            ("y", timing(0.2, 0.5)),
        ]);
        assert_eq!(split_units(&mut units, &profile), 1);
        assert!(units.iter().any(|u| u.key == "a#meta"));
        assert_eq!(by_key(&units, "y").dep_extern[0].1, "x");
    }

    /// Two libraries hold up two binaries that take equally long.  Each
    /// split alone leaves the build as long as before, and only both
    /// together shorten it, so neither may be undone.
    #[test]
    fn splits_that_only_help_together_are_kept() {
        let mut units = vec![
            unit("a", "lib", &[]),
            unit("b", "lib", &["a"]),
            unit("c", "bin", &["a", "b"]),
            unit("x", "lib", &[]),
            unit("y", "lib", &["x"]),
            unit("z", "bin", &["x", "y"]),
        ];
        let profile = profile(&[
            ("a", timing(0.5, 3.0)),
            ("b", timing(0.5, 1.0)),
            ("x", timing(0.5, 3.0)),
            ("y", timing(0.5, 1.0)),
        ]);
        assert_eq!(split_units(&mut units, &profile), 2);
    }

    #[test]
    fn cost_is_cpu_time_up_to_the_wall_time() {
        let at = |wall, cpu| UnitTiming {
            wall,
            cpu,
            frontend: None,
        };
        assert_eq!(at(3.0, None).secs(), 3.0);
        assert_eq!(at(3.0, Some(2.0)).secs(), 2.0);
        assert_eq!(at(24.0, Some(110.0)).secs(), 24.0);
    }

    #[test]
    fn split_that_saves_too_little_is_undone() {
        let mut units = chain();
        let profile = profile(&[("a", timing(0.02, 0.1)), ("b", timing(1.0, 2.0))]);
        assert_eq!(split_units(&mut units, &profile), 0);
    }

    /// A library that only binaries depend on gains nothing, because a
    /// linking unit needs its object code either way.
    #[test]
    fn library_without_library_dependent_is_not_split() {
        let mut units = vec![unit("a", "lib", &[]), unit("c", "bin", &["a"])];
        let profile = profile(&[("a", timing(1.0, 5.0))]);
        assert_eq!(split_units(&mut units, &profile), 0);
    }

    #[test]
    fn profile_keeps_a_library_apart_from_its_binary() {
        let mut units = [unit("a", "lib", &[]), unit("a", "bin", &["a"])];
        units[1].key = "a-bin".into();
        let profile = PipelineProfile {
            libraries: [("a-0.1.0-a".into(), timing(1.0, 5.0))].into(),
            others: [("a-0.1.0-a".into(), timing(0.5, 9.0))].into(),
        };
        assert_eq!(profile.timing(&units[0]).unwrap().wall, 5.0);
        assert_eq!(profile.timing(&units[1]).unwrap().wall, 9.0);
    }

    /// Recording a split build that was planned with a profile replaces the
    /// units it ran, keeps those that came from the cache, and leaves out
    /// metadata halves.
    #[test]
    fn recorder_updates_the_profile_a_split_build_was_planned_with() {
        let event = |json: serde_json::Value| format!("@nix {json}");
        let started = |id: u64, name: &str, us: u64| {
            event(serde_json::json!({
                "action": "start", "fields": [format!("/nix/store/h-{name}.drv"), "", 1, 1],
                "id": id, "level": 3, "text": "building", "ts": us, "type": 105,
            }))
        };
        let artifact = |id: u64, file: &str, emit: &str, us: u64| {
            let notice = serde_json::json!({
                "$message_type": "artifact", "artifact": format!("/nix/store/o/{file}"), "emit": emit,
            });
            event(serde_json::json!({
                "action": "result", "fields": [notice.to_string()], "id": id, "ts": us, "type": 101,
            }))
        };
        let stopped =
            |id: u64, us: u64| event(serde_json::json!({"action": "stop", "id": id, "ts": us}));

        let planned_with = profile(&[("a", timing(1.0, 9.0)), ("z", timing(1.0, 2.0))]);
        let mut recorder = PipelineRecorder::new(Instant::now(), planned_with);
        recorder.read(&started(1, "a-0.1.0-a-meta", 0));
        recorder.read(&started(2, "a-0.1.0-a", 0));
        recorder.read(&artifact(1, "liba.rmeta", "metadata", 2_000_000));
        recorder.read(&artifact(2, "liba.rmeta", "metadata", 2_500_000));
        recorder.read(&stopped(1, 2_100_000));
        recorder.read(&artifact(2, "liba.rlib", "link", 5_000_000));
        recorder.read(&stopped(2, 5_000_000));

        let profile = recorder.finish();
        assert_eq!(profile.libraries["a-0.1.0-a"], timing(2.5, 5.0));
        assert_eq!(profile.libraries["z-0.1.0-z"], timing(1.0, 2.0));
        assert!(profile.others.is_empty());
    }

    #[test]
    fn recorder_times_builds_by_activity_on_nix_clock() {
        let started = |id: u64, name: &str, us: u64| {
            format!(
                r#"@nix {{"action":"start","fields":["/nix/store/h-{name}.drv","",1,1],"id":{id},"level":3,"text":"building '/nix/store/h-{name}.drv'","ts":{us},"type":105}}"#
            )
        };
        let artifact = |id: u64, file: &str, emit: &str, us: u64| {
            let notice = format!(
                r#"{{"$message_type":"artifact","artifact":"/nix/store/o-a-0.1.0-a/{file}","emit":"{emit}"}}"#
            );
            let line = serde_json::json!({
                "action": "result", "fields": [notice], "id": id, "ts": us, "type": 101,
            });
            format!("@nix {line}")
        };
        let resources = |id: u64| {
            format!(
                r#"@nix {{"action":"result","fields":["cpu-user-us",6000000,"cpu-system-us",500000],"id":{id},"type":1001}}"#
            )
        };
        let stopped = |id: u64, us: u64| format!(r#"@nix {{"action":"stop","id":{id},"ts":{us}}}"#);

        let mut recorder = PipelineRecorder::new(Instant::now(), PipelineProfile::default());
        assert_eq!(
            recorder.read(&started(1, "a-0.1.0-a", 10_000_000)),
            vec!["building '/nix/store/h-a-0.1.0-a.drv'".to_string()]
        );
        recorder.read(&started(2, "a-0.1.0-a", 11_000_000));
        assert!(
            recorder
                .read(&artifact(1, "liba.rmeta", "metadata", 11_000_000))
                .is_empty()
        );
        recorder.read(&artifact(1, "liba.rlib", "link", 14_000_000));
        recorder.read(&resources(1));
        recorder.read(&stopped(1, 14_500_000));
        recorder.read(&artifact(2, "a.rmeta", "metadata", 11_500_000));
        recorder.read(&artifact(2, "a", "link", 12_000_000));
        recorder.read(&stopped(2, 13_000_000));

        let profile = recorder.finish();
        assert_eq!(
            profile.libraries["a-0.1.0-a"],
            UnitTiming {
                wall: 4.5,
                cpu: Some(6.5),
                frontend: Some(1.0),
            }
        );
        assert_eq!(
            profile.others["a-0.1.0-a"],
            UnitTiming {
                wall: 2.0,
                cpu: None,
                frontend: Some(0.5),
            }
        );
    }
}
