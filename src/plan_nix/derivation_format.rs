//! Strongly-typed JSON shapes for `nix derivation add` across the Nix 2.x
//! schema timeline, plus runtime detection of which shape the local store
//! accepts.
//!
//! cargo-schnee's internal IR — built by [`construct_derivation`] in
//! `derivation.rs` — predates the schema-versioning era. Conversion to the
//! externally observable shape happens in this module via the
//! [`NixDerivation`] tagged union: [`PreNix232`](NixDerivation::PreNix232),
//! [`Nix232`](NixDerivation::Nix232), and [`Nix233`](NixDerivation::Nix233).
//! Each variant is a concrete Rust struct that serialises to *exactly* the
//! shape that release of Nix expects, so the wire format is enforced by the
//! type system rather than by a fragile post-hoc rewrite of a `Value`.
//!
//! ## Schema timeline
//!
//! ### Pre-Nix 2.32 — `PreNix232`
//! No `"version"` key. Top-level `inputDrvs` / `inputSrcs`. Store paths
//! include the active store directory prefix (`/nix/store/...` for the
//! default store) in structural positions.
//!
//! ### Nix 2.32 — `Nix232` introduces `"version": 3` and basename store paths
//! - Release notes:
//!   <https://github.com/NixOS/nix/blob/master/doc/manual/source/release-notes/rl-2.32.md>
//!   §"Derivation JSON format now uses store path basenames only".
//! - Tracking issue: <https://github.com/NixOS/nix/issues/13570>.
//! - PR: <https://github.com/NixOS/nix/pull/13980>.
//! - Source change: commit `9d7229a2a` "Make the JSON format for derivation
//!   use basename store paths" —
//!   <https://github.com/NixOS/nix/commit/9d7229a2a429b7de0e392d40f222d3d2802989da>.
//!
//! ### Nix 2.33 — `Nix233` introduces `"version": 4` and nested `inputs`
//! - Release notes:
//!   <https://github.com/NixOS/nix/blob/master/doc/manual/source/release-notes/rl-2.33.md>
//!   §"Derivation JSON format changes". Quote: "Version 3 and earlier
//!   formats are *not* accepted when reading."
//! - Source change: commit `0c37a6220` "Change JSON derivation format in two
//!   ways" —
//!   <https://github.com/NixOS/nix/commit/0c37a62207f52aa9d58b8d18a4860dec3a270f72>.
//!   Reorganises `inputSrcs` / `inputDrvs` into `inputs.{srcs, drvs}` and
//!   switches fixed-CA outputs to the canonical `ContentAddress` shape (the
//!   latter does not affect cargo-schnee, which emits floating CA outputs
//!   only).
//! - JSON Schema:
//!   <https://github.com/NixOS/nix/blob/master/doc/manual/source/protocols/json/schema/derivation-v4.yaml>.
//! - Authoritative version constant in the Nix source tree:
//!   <https://github.com/NixOS/nix/blob/master/src/libstore/include/nix/store/derivations.hh#L618>
//!   — `constexpr unsigned expectedJsonVersionDerivation = 4;`. Decoder is
//!   `Derivation::from_json` in `src/libstore/derivations.cc`.

use anyhow::{Context, Result};
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::OnceLock;

use super::daemon::NixDaemonConn;

// ---------------------------------------------------------------------------
// Store directory
// ---------------------------------------------------------------------------

/// The active Nix store directory.
///
/// Resolved at runtime from `NIX_STORE_DIR`, falling back to the upstream
/// default `/nix/store`. The worker protocol does not expose a direct
/// "query store dir" opcode; the env var is what every Nix client (libstore,
/// nix-store, etc.) consults first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StoreDir(String);

impl StoreDir {
    pub(super) fn detect() -> Self {
        Self(std::env::var("NIX_STORE_DIR").unwrap_or_else(|_| "/nix/store".into()))
    }

    #[cfg(test)]
    pub(super) fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// Strip this store dir's prefix and any trailing slash from `path`.
    /// Returns `path` unchanged if the prefix does not match — callers
    /// upstream guarantee structural store paths are always rooted at the
    /// active store, but for robustness we never split on a non-match.
    pub(super) fn basename<'a>(&self, path: &'a str) -> &'a str {
        match path.strip_prefix(self.0.as_str()) {
            Some(rest) => rest.strip_prefix('/').unwrap_or(rest),
            None => path,
        }
    }
}

// ---------------------------------------------------------------------------
// Target Nix release
// ---------------------------------------------------------------------------

/// Which Nix release the local `nix derivation add` is from. Determines
/// which [`NixDerivation`] variant we must produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetNix {
    PreNix232,
    Nix232,
    Nix233,
}

impl TargetNix {
    /// Pure mapping from a parsed `(major, minor)` to schema target. Future
    /// Nix releases default to `Nix233` until we learn of another breaking
    /// change; bump this and the `version_dispatch` test in the same commit.
    pub(super) fn for_version(major: u32, minor: u32) -> Self {
        match (major, minor) {
            (2, m) if m < 32 => Self::PreNix232,
            (2, 32) => Self::Nix232,
            _ => Self::Nix233,
        }
    }

    /// Detect once per process. Prefers the daemon handshake, falling back
    /// to `nix --version` only if the daemon is unreachable or its protocol
    /// predates 1.33 (when the version string was added to the handshake).
    pub(super) fn detect() -> Result<Self> {
        static CACHED: OnceLock<TargetNix> = OnceLock::new();
        if let Some(t) = CACHED.get() {
            return Ok(*t);
        }
        let t = detect_uncached()?;
        let _ = CACHED.set(t);
        Ok(t)
    }
}

fn detect_uncached() -> Result<TargetNix> {
    if let Some(version) = detect_via_daemon() {
        debug!("Detected Nix version via daemon handshake: {}", version);
        let (major, minor) = parse_version(&version)
            .with_context(|| format!("Could not parse Nix version from daemon: {:?}", version))?;
        return Ok(TargetNix::for_version(major, minor));
    }
    debug!("Daemon unavailable or pre-1.33; falling back to `nix --version`");
    let version = run_nix_version_cli()?;
    let (major, minor) = parse_version(&version).with_context(|| {
        format!("Could not parse Nix version from `nix --version`: {:?}", version)
    })?;
    Ok(TargetNix::for_version(major, minor))
}

fn detect_via_daemon() -> Option<String> {
    let conn = NixDaemonConn::connect().ok()?;
    conn.nix_version().map(str::to_owned)
}

fn run_nix_version_cli() -> Result<String> {
    let output = Command::new("nix")
        .arg("--version")
        .output()
        .context("running `nix --version`")?;
    if !output.status.success() {
        anyhow::bail!(
            "`nix --version` exited non-zero: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Extract `(major, minor)` from either a bare daemon version string
/// (`"2.34.7"`) or the `nix --version` CLI line
/// (`"nix (Nix) 2.34.7"`, `"lix (Lix, like Nix) 2.93.0"`).
///
/// Picks the first whitespace-delimited token that begins with a digit and
/// contains a dot.
fn parse_version(s: &str) -> Option<(u32, u32)> {
    let token = s.split_whitespace().find(|t| {
        t.chars().next().is_some_and(|c| c.is_ascii_digit()) && t.contains('.')
    })?;
    let mut parts = token.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

// ---------------------------------------------------------------------------
// Shared sub-shapes
// ---------------------------------------------------------------------------

/// Per-input-derivation outputs reference. Same shape across all three
/// schema versions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct InputDrv {
    pub outputs: Vec<String>,
    #[serde(rename = "dynamicOutputs", default)]
    pub dynamic_outputs: BTreeMap<String, serde_json::Value>,
}

/// Output specification. cargo-schnee only emits floating-CA outputs whose
/// shape (`{hashAlgo, method}`) is unchanged across all three formats.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct OutputSpec {
    #[serde(rename = "hashAlgo")]
    pub hash_algo: String,
    pub method: String,
}

// ---------------------------------------------------------------------------
// Per-version typed shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct PreNix232Derivation {
    pub name: String,
    pub system: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    #[serde(rename = "inputDrvs")]
    pub input_drvs: BTreeMap<String, InputDrv>,
    #[serde(rename = "inputSrcs")]
    pub input_srcs: Vec<String>,
    pub outputs: BTreeMap<String, OutputSpec>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct Nix232Derivation {
    pub name: String,
    /// Always `3`. Enforced by the constructor.
    pub version: u8,
    pub system: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    #[serde(rename = "inputDrvs")]
    pub input_drvs: BTreeMap<String, InputDrv>,
    #[serde(rename = "inputSrcs")]
    pub input_srcs: Vec<String>,
    pub outputs: BTreeMap<String, OutputSpec>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct Nix233Derivation {
    pub name: String,
    /// Always `4`. Enforced by the constructor.
    pub version: u8,
    pub system: String,
    pub builder: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub inputs: Nix233Inputs,
    pub outputs: BTreeMap<String, OutputSpec>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(super) struct Nix233Inputs {
    pub drvs: BTreeMap<String, InputDrv>,
    pub srcs: Vec<String>,
}

/// Tagged union over the three concrete derivation shapes. `#[serde(untagged)]`
/// makes JSON serialisation flatten to the inner struct's shape, so
/// `serde_json::to_string(&NixDerivation::Nix233(...))` produces exactly the
/// v4 JSON object — no enum wrapping.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub(super) enum NixDerivation {
    PreNix232(PreNix232Derivation),
    Nix232(Nix232Derivation),
    Nix233(Nix233Derivation),
}

// ---------------------------------------------------------------------------
// Conversion from cargo-schnee's internal IR
// ---------------------------------------------------------------------------

/// The shape of cargo-schnee's internal IR built by `construct_derivation`.
/// Treated as the canonical input to all three external shapes.
#[derive(Debug, Clone, Deserialize)]
struct InternalIr {
    name: String,
    system: String,
    builder: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    #[serde(rename = "inputDrvs")]
    input_drvs: BTreeMap<String, InputDrv>,
    #[serde(rename = "inputSrcs")]
    input_srcs: Vec<String>,
    outputs: BTreeMap<String, OutputSpec>,
}

impl NixDerivation {
    /// Convert cargo-schnee's internal IR JSON into the typed shape that
    /// `target` requires. `store` is used to rewrite structural store paths
    /// to basenames for `Nix232` and `Nix233`; for `PreNix232` the IR is
    /// passed through verbatim.
    pub(super) fn from_ir(
        ir: &serde_json::Value,
        target: TargetNix,
        store: &StoreDir,
    ) -> Result<Self> {
        let parsed: InternalIr =
            serde_json::from_value(ir.clone()).context("Parsing internal IR derivation JSON")?;
        Ok(match target {
            TargetNix::PreNix232 => Self::PreNix232(PreNix232Derivation {
                name: parsed.name,
                system: parsed.system,
                builder: parsed.builder,
                args: parsed.args,
                env: parsed.env,
                input_drvs: parsed.input_drvs,
                input_srcs: parsed.input_srcs,
                outputs: parsed.outputs,
            }),
            TargetNix::Nix232 => Self::Nix232(Nix232Derivation {
                name: parsed.name,
                version: 3,
                system: parsed.system,
                builder: parsed.builder,
                args: parsed.args,
                env: parsed.env,
                input_drvs: rebase_drv_keys(parsed.input_drvs, store),
                input_srcs: rebase_srcs(parsed.input_srcs, store),
                outputs: parsed.outputs,
            }),
            TargetNix::Nix233 => Self::Nix233(Nix233Derivation {
                name: parsed.name,
                version: 4,
                system: parsed.system,
                builder: parsed.builder,
                args: parsed.args,
                env: parsed.env,
                inputs: Nix233Inputs {
                    drvs: rebase_drv_keys(parsed.input_drvs, store),
                    srcs: rebase_srcs(parsed.input_srcs, store),
                },
                outputs: parsed.outputs,
            }),
        })
    }
}

fn rebase_drv_keys(
    drvs: BTreeMap<String, InputDrv>,
    store: &StoreDir,
) -> BTreeMap<String, InputDrv> {
    drvs.into_iter()
        .map(|(k, v)| (store.basename(&k).to_owned(), v))
        .collect()
}

fn rebase_srcs(srcs: Vec<String>, store: &StoreDir) -> Vec<String> {
    srcs.into_iter()
        .map(|p| store.basename(&p).to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- TargetNix dispatch -------------------------------------------------

    #[test]
    fn version_dispatch() {
        assert_eq!(TargetNix::for_version(2, 18), TargetNix::PreNix232);
        assert_eq!(TargetNix::for_version(2, 31), TargetNix::PreNix232);
        assert_eq!(TargetNix::for_version(2, 32), TargetNix::Nix232);
        assert_eq!(TargetNix::for_version(2, 33), TargetNix::Nix233);
        assert_eq!(TargetNix::for_version(2, 34), TargetNix::Nix233);
        assert_eq!(TargetNix::for_version(3, 0), TargetNix::Nix233);
    }

    #[test]
    fn parses_daemon_and_cli_version_strings() {
        // Daemon protocol >= 1.33 sends a bare version string.
        assert_eq!(parse_version("2.34.7"), Some((2, 34)));
        assert_eq!(parse_version("2.31.3\n"), Some((2, 31)));
        // `nix --version` CLI output.
        assert_eq!(parse_version("nix (Nix) 2.31.3\n"), Some((2, 31)));
        assert_eq!(parse_version("nix (Nix) 2.34.7"), Some((2, 34)));
        // Lix CLI marker.
        assert_eq!(parse_version("lix (Lix, like Nix) 2.93.0"), Some((2, 93)));
        // Edge cases — fail loudly rather than guess.
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("nix-2.18.1\n"), None);
    }

    // --- StoreDir -----------------------------------------------------------

    #[test]
    fn store_dir_strips_default_prefix() {
        let s = StoreDir::new("/nix/store");
        assert_eq!(s.basename("/nix/store/abc-foo"), "abc-foo");
        assert_eq!(s.basename("/nix/store/abc-foo.drv"), "abc-foo.drv");
        // Non-matching prefix is passed through untouched.
        assert_eq!(s.basename("not-a-store-path"), "not-a-store-path");
    }

    #[test]
    fn store_dir_strips_custom_prefix() {
        let s = StoreDir::new("/opt/nix/store");
        assert_eq!(s.basename("/opt/nix/store/zzz-bar"), "zzz-bar");
        // The default prefix must NOT be stripped against a custom store.
        assert_eq!(s.basename("/nix/store/zzz-bar"), "/nix/store/zzz-bar");
    }

    // --- Conversion ---------------------------------------------------------

    fn ir(store: &str) -> serde_json::Value {
        json!({
            "name": "anstyle-1.0.14-anstyle",
            "system": "x86_64-linux",
            "builder": format!("{}/aaa-bash/bin/bash", store),
            "args": ["-c", "exit 0"],
            "env": { "out": "placeholder" },
            "inputDrvs": {
                format!("{}/bbb-coreutils.drv", store): {
                    "dynamicOutputs": {},
                    "outputs": ["out"]
                }
            },
            "inputSrcs": [
                format!("{}/ccc-glibc", store),
                format!("{}/ddd-vendor", store)
            ],
            "outputs": { "out": { "hashAlgo": "sha256", "method": "nar" } }
        })
    }

    #[test]
    fn pre_nix232_passthrough_keeps_full_paths() {
        let store = StoreDir::new("/nix/store");
        let drv = NixDerivation::from_ir(&ir("/nix/store"), TargetNix::PreNix232, &store).unwrap();
        let v = serde_json::to_value(&drv).unwrap();

        // No version key; structural store paths stay full.
        assert!(v.as_object().unwrap().get("version").is_none());
        assert!(
            v["inputDrvs"]
                .as_object()
                .unwrap()
                .contains_key("/nix/store/bbb-coreutils.drv")
        );
        let srcs = v["inputSrcs"].as_array().unwrap();
        assert_eq!(srcs[0], json!("/nix/store/ccc-glibc"));
        assert_eq!(srcs[1], json!("/nix/store/ddd-vendor"));
    }

    #[test]
    fn nix232_strips_store_dir_and_stamps_version_3() {
        let store = StoreDir::new("/nix/store");
        let drv = NixDerivation::from_ir(&ir("/nix/store"), TargetNix::Nix232, &store).unwrap();
        let v = serde_json::to_value(&drv).unwrap();

        assert_eq!(v["version"], json!(3));
        assert!(
            v["inputDrvs"]
                .as_object()
                .unwrap()
                .contains_key("bbb-coreutils.drv")
        );
        let srcs = v["inputSrcs"].as_array().unwrap();
        assert_eq!(srcs[0], json!("ccc-glibc"));
        assert_eq!(srcs[1], json!("ddd-vendor"));

        // Builder and args stay verbatim — store paths there are content,
        // not structural; the JSON guideline rewrite only touched structural
        // occurrences.
        assert_eq!(v["builder"], json!("/nix/store/aaa-bash/bin/bash"));
        assert_eq!(v["args"], json!(["-c", "exit 0"]));
    }

    #[test]
    fn nix233_nests_inputs_and_stamps_version_4() {
        let store = StoreDir::new("/nix/store");
        let drv = NixDerivation::from_ir(&ir("/nix/store"), TargetNix::Nix233, &store).unwrap();
        let v = serde_json::to_value(&drv).unwrap();

        assert_eq!(v["version"], json!(4));
        let obj = v.as_object().unwrap();
        assert!(obj.get("inputDrvs").is_none());
        assert!(obj.get("inputSrcs").is_none());

        let inputs = v["inputs"].as_object().unwrap();
        assert!(
            inputs["drvs"]
                .as_object()
                .unwrap()
                .contains_key("bbb-coreutils.drv")
        );
        let srcs = inputs["srcs"].as_array().unwrap();
        assert_eq!(srcs[0], json!("ccc-glibc"));
        assert_eq!(srcs[1], json!("ddd-vendor"));

        // Inner shape preserved: v4 keeps `{outputs, dynamicOutputs}` per drv.
        let dep = &inputs["drvs"]["bbb-coreutils.drv"];
        assert_eq!(dep["outputs"], json!(["out"]));
        assert_eq!(dep["dynamicOutputs"], json!({}));
    }

    /// The v4 fixture distributed in the Nix test suite at
    /// `src/json-schema-checks/derivation/simple-derivation.json` carries
    /// the canonical top-level key set. Pin our v4 output against it.
    #[test]
    fn nix233_top_level_keys_match_upstream_schema() {
        let store = StoreDir::new("/nix/store");
        let drv = NixDerivation::from_ir(&ir("/nix/store"), TargetNix::Nix233, &store).unwrap();
        let v = serde_json::to_value(&drv).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["args", "builder", "env", "inputs", "name", "outputs", "system", "version"],
        );
    }

    #[test]
    fn conversion_honours_custom_store_dir() {
        let store = StoreDir::new("/opt/nix/store");
        let drv = NixDerivation::from_ir(&ir("/opt/nix/store"), TargetNix::Nix233, &store).unwrap();
        let v = serde_json::to_value(&drv).unwrap();

        let inputs = v["inputs"].as_object().unwrap();
        assert!(
            inputs["drvs"]
                .as_object()
                .unwrap()
                .contains_key("bbb-coreutils.drv"),
            "basename should be stripped against the custom store dir, got: {:?}",
            inputs["drvs"],
        );
        assert_eq!(inputs["srcs"][0], json!("ccc-glibc"));
    }
}
