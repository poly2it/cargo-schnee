//! Output paths of already built units, looked up in the daemon's build
//! trace.
//!
//! Every unit derivation is content-addressed with floating outputs, so Nix
//! resolves a derivation with inputs before building it: it substitutes each
//! input's realised output for that input's placeholder and builds the
//! resolved derivation instead. The build trace then holds an entry for the
//! resolved derivation only. Looking up the registered derivation therefore
//! finds nothing, and this module repeats Nix's resolution, as in
//! `Derivation::tryResolve`, before it queries the trace.

use super::aterm::{collect_drv_refs, compute_drv_store_path, serialize_derivation_aterm};
use super::daemon::NixDaemonConn;
use super::derivation::downstream_placeholder;
use super::{NixUnit, UnitKind};
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};

/// Output paths of the local units whose compiler diagnostics a build
/// shows, in plan order. A unit without a build trace entry is left out,
/// with one warning per call that counts the units left out.
///
/// Errors when the daemon cannot be reached or keys its build trace by a
/// hash modulo rather than by derivation path, as daemons from before the
/// build-trace rework do.
pub fn realise_local_outputs(units: &[NixUnit]) -> Result<Vec<String>> {
    let mut conn = NixDaemonConn::connect()?;
    anyhow::ensure!(
        conn.has_path_keyed_build_trace(),
        "the Nix daemon does not key its build trace by derivation path"
    );
    let jsons: HashMap<&str, &serde_json::Value> = units
        .iter()
        .filter_map(|u| Some((u.drv_path.as_deref()?, u.drv_json.as_ref()?)))
        .collect();
    let (out, miss) = local_outputs(local_diagnostic_drv_paths(units), jsons, |key| {
        conn.query_realisation(key, "out")
    })?;
    if let Some(miss) = miss {
        tracing::warn!("{miss}");
    }
    Ok(out)
}

/// Local units left without a build trace entry. The build has just
/// succeeded, so every unit's resolved derivation has one, and a miss means
/// this module resolves differently from Nix.
#[derive(Debug)]
struct TraceMiss {
    /// Local units whose diagnostics are not replayed.
    units: usize,
    /// The first derivation whose own lookup missed.
    drv_path: String,
    /// The path that derivation was looked up as.
    key: String,
}

impl std::fmt::Display for TraceMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Not replaying the compiler diagnostics of {} local unit(s): no build trace \
             entry for {}, looked up as {}. cargo-schnee's derivation resolution no \
             longer matches Nix's.",
            self.units, self.drv_path, self.key,
        )
    }
}

/// [`realise_local_outputs`] with the build trace lookup supplied by
/// `query`, which takes the derivation path the trace is keyed by.
fn local_outputs<'a>(
    local: impl Iterator<Item = &'a str>,
    jsons: HashMap<&'a str, &'a serde_json::Value>,
    query: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<(Vec<String>, Option<TraceMiss>)> {
    let mut trace = BuildTrace {
        query,
        jsons,
        outputs: HashMap::new(),
        first_miss: None,
    };
    let mut out = Vec::new();
    let mut missed = 0usize;
    for drv_path in local {
        match trace.output(drv_path)? {
            Some(path) => out.push(path),
            None => missed += 1,
        }
    }
    let miss = trace.first_miss.map(|(drv_path, key)| TraceMiss {
        units: missed,
        drv_path,
        key,
    });
    Ok((out, miss))
}

/// Derivation paths of local units whose outputs carry a `diagnostics` file.
fn local_diagnostic_drv_paths(units: &[NixUnit]) -> impl Iterator<Item = &str> {
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
}

struct BuildTrace<'a, Q> {
    query: Q,
    jsons: HashMap<&'a str, &'a serde_json::Value>,
    /// Output path per registered derivation path, `None` when a
    /// derivation or one of its inputs has no build trace entry.
    outputs: HashMap<String, Option<String>>,
    /// The first derivation whose own lookup missed, with the path it was
    /// looked up as.
    first_miss: Option<(String, String)>,
}

impl<Q: FnMut(&str) -> Result<Option<String>>> BuildTrace<'_, Q> {
    fn output(&mut self, drv_path: &str) -> Result<Option<String>> {
        if let Some(known) = self.outputs.get(drv_path) {
            return Ok(known.clone());
        }
        let json = *self
            .jsons
            .get(drv_path)
            .with_context(|| format!("no derivation recorded for {drv_path}"))?;
        let mut inputs = HashMap::new();
        for input in input_drv_paths(json)? {
            match self.output(&input)? {
                Some(path) => inputs.insert(input, path),
                None => return Ok(self.remember(drv_path, None)),
            };
        }
        let key = if inputs.is_empty() {
            drv_path.to_string()
        } else {
            resolved_drv_path(drv_path, json, &inputs)?
        };
        let path = (self.query)(&key)?;
        if path.is_none() && self.first_miss.is_none() {
            self.first_miss = Some((drv_path.to_string(), key));
        }
        Ok(self.remember(drv_path, path))
    }

    fn remember(&mut self, drv_path: &str, path: Option<String>) -> Option<String> {
        self.outputs.insert(drv_path.to_string(), path.clone());
        path
    }
}

/// Input derivations of `json`. Every unit consumes exactly the `out`
/// output of each input, which is all [`resolve`] handles.
fn input_drv_paths(json: &serde_json::Value) -> Result<Vec<String>> {
    let input_drvs = json["inputDrvs"]
        .as_object()
        .context("derivation JSON missing 'inputDrvs' object")?;
    input_drvs
        .iter()
        .map(|(path, info)| {
            let outputs_only_out = info["outputs"]
                .as_array()
                .is_some_and(|o| o.len() == 1 && o[0].as_str() == Some("out"));
            let no_dynamic = info["dynamicOutputs"]
                .as_object()
                .is_none_or(|d| d.is_empty());
            anyhow::ensure!(
                outputs_only_out && no_dynamic,
                "input {path} is consumed through outputs other than 'out'"
            );
            Ok(path.clone())
        })
        .collect()
}

/// Store path of the derivation Nix builds in place of `drv_path` once
/// each input derivation's `out` has been realised at `inputs[input]`.
fn resolved_drv_path(
    drv_path: &str,
    json: &serde_json::Value,
    inputs: &HashMap<String, String>,
) -> Result<String> {
    let resolved = resolve(json, inputs)?;
    let aterm = serialize_derivation_aterm(&resolved)?;
    let refs = collect_drv_refs(&resolved);
    let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
    let name = drv_path
        .strip_prefix("/nix/store/")
        .and_then(|base| base.get(33..))
        .with_context(|| format!("malformed derivation path {drv_path}"))?;
    Ok(compute_drv_store_path(name, &aterm, &ref_strs))
}

/// The resolved form of `json`: input derivations become input sources at
/// their realised paths, and every placeholder for an input's output is
/// rewritten to that path in the builder, the arguments, and the names and
/// values of the environment.
fn resolve(
    json: &serde_json::Value,
    inputs: &HashMap<String, String>,
) -> Result<serde_json::Value> {
    let rewrites: Vec<(String, &str)> = inputs
        .iter()
        .map(|(drv, path)| Ok((downstream_placeholder(drv, "out")?, path.as_str())))
        .collect::<Result<_>>()?;
    let rewrite = |s: &str| {
        rewrites.iter().fold(s.to_string(), |acc, (from, to)| {
            acc.replace(from.as_str(), to)
        })
    };

    let mut srcs: BTreeSet<String> = json["inputSrcs"]
        .as_array()
        .context("derivation JSON missing 'inputSrcs' array")?
        .iter()
        .map(|s| {
            s.as_str()
                .map(str::to_string)
                .context("non-string in inputSrcs")
        })
        .collect::<Result<_>>()?;
    srcs.extend(inputs.values().cloned());

    let builder = json["builder"]
        .as_str()
        .context("derivation JSON missing 'builder' string")?;
    let args: Vec<serde_json::Value> = json["args"]
        .as_array()
        .context("derivation JSON missing 'args' array")?
        .iter()
        .map(|a| Ok(rewrite(a.as_str().context("non-string in args array")?).into()))
        .collect::<Result<_>>()?;
    let env: serde_json::Map<String, serde_json::Value> = json["env"]
        .as_object()
        .context("derivation JSON missing 'env' object")?
        .iter()
        .map(|(k, v)| {
            let v = v
                .as_str()
                .with_context(|| format!("non-string env value for key '{k}'"))?;
            Ok((rewrite(k), rewrite(v).into()))
        })
        .collect::<Result<_>>()?;

    let mut resolved = json.clone();
    resolved["inputDrvs"] = serde_json::Value::Object(serde_json::Map::new());
    resolved["inputSrcs"] = srcs.into_iter().map(serde_json::Value::String).collect();
    resolved["builder"] = rewrite(builder).into();
    resolved["args"] = args.into();
    resolved["env"] = env.into();
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit derivation and its resolution as Nix 2.35 performed it,
    /// captured from a build of a two-crate workspace where `warn-bin`
    /// depends on `warn-lib`.
    #[derive(serde::Deserialize)]
    struct Captured {
        drv_path: String,
        json: serde_json::Value,
        inputs: HashMap<String, String>,
        resolved_drv_path: String,
    }

    fn captured() -> Captured {
        serde_json::from_str(include_str!("testdata/resolve-warn-bin.json")).unwrap()
    }

    #[test]
    fn resolved_drv_path_matches_nix() {
        let c = captured();
        let aterm = serialize_derivation_aterm(&c.json).unwrap();
        let refs = collect_drv_refs(&c.json);
        let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
        assert_eq!(
            compute_drv_store_path("warn-bin-0.1.0-warn-bin.drv", &aterm, &ref_strs),
            c.drv_path,
        );
        assert_eq!(
            resolved_drv_path(&c.drv_path, &c.json, &c.inputs).unwrap(),
            c.resolved_drv_path,
        );
    }

    #[test]
    fn resolve_replaces_every_input_placeholder() {
        let c = captured();
        let resolved = resolve(&c.json, &c.inputs).unwrap();
        let text = resolved.to_string();
        for (drv, path) in &c.inputs {
            let placeholder = downstream_placeholder(drv, "out").unwrap();
            assert!(c.json.to_string().contains(&placeholder));
            assert!(!text.contains(&placeholder));
            assert!(!text.contains(drv.as_str()));
            assert!(
                resolved["inputSrcs"]
                    .as_array()
                    .unwrap()
                    .contains(&path.as_str().into())
            );
        }
        assert!(resolved["inputDrvs"].as_object().unwrap().is_empty());
    }

    /// Runs [`local_outputs`] for the captured `warn-bin` unit against a
    /// build trace that holds exactly what Nix recorded.
    fn replay_captured(json: &serde_json::Value) -> (Vec<String>, Option<TraceMiss>) {
        let c = captured();
        let (lib_drv, lib_out) = c.inputs.iter().next().unwrap();
        let lib_json = serde_json::json!({"inputDrvs": {}});
        let jsons = HashMap::from([(lib_drv.as_str(), &lib_json), (c.drv_path.as_str(), json)]);
        let trace = HashMap::from([
            (lib_drv.clone(), lib_out.clone()),
            (
                c.resolved_drv_path.clone(),
                "/nix/store/x-warn-bin".to_string(),
            ),
        ]);

        local_outputs(
            [lib_drv.as_str(), c.drv_path.as_str()].into_iter(),
            jsons,
            |key| Ok(trace.get(key).cloned()),
        )
        .unwrap()
    }

    #[test]
    fn matching_resolution_reports_no_miss() {
        let (out, miss) = replay_captured(&captured().json);
        assert_eq!(out.len(), 2);
        assert!(miss.is_none(), "unexpected miss: {miss:?}");
    }

    #[test]
    fn drifted_resolution_reports_the_miss() {
        let c = captured();
        let mut json = c.json.clone();
        json["env"]["drifted"] = "1".into();
        let (out, miss) = replay_captured(&json);
        assert_eq!(out.len(), 1);
        let miss = miss.expect("a drifted resolution must report a miss");
        assert_eq!(miss.units, 1);
        assert_eq!(miss.drv_path, c.drv_path);
        assert_ne!(miss.key, c.resolved_drv_path);
        let warning = miss.to_string();
        assert!(warning.contains("of 1 local unit(s)"), "{warning}");
        assert!(warning.contains(&c.drv_path), "{warning}");
    }

    #[test]
    fn input_consumed_through_other_outputs_is_rejected() {
        let json = serde_json::json!({
            "inputDrvs": {"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv": {
                "outputs": ["dev"], "dynamicOutputs": {}
            }}
        });
        assert!(input_drv_paths(&json).is_err());
    }
}
