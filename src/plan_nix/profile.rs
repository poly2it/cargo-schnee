//! Per-unit codegen settings, taken from the profile Cargo resolved for each
//! unit so every derivation compiles with the flags `cargo build` would pass.

use anyhow::{Result, bail};
use cargo::core::compiler::{BuildContext, CompileMode, CrateType, Lto, Unit};
use cargo::core::profiles::{self, PanicStrategy, ProfileRoot, StripInner};
use std::collections::HashMap;
use std::collections::hash_map::Entry;

/// The codegen part of one unit's Cargo profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct UnitProfile {
    /// The profile-derived rustc flags, in the order Cargo's
    /// `build_base_args` emits them.
    pub(crate) rustc_args: Vec<String>,
    /// `OPT_LEVEL` for a build-script run.
    pub(crate) opt_level: String,
    /// `DEBUG` for a build-script run.
    pub(crate) debug: bool,
    /// `PROFILE` for a build-script run, `release` or `debug`.
    pub(crate) root: String,
}

impl UnitProfile {
    /// Every field in one string, for unit keys and `-C metadata`.
    pub(crate) fn identity(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.rustc_args.join(" "),
            self.opt_level,
            self.debug,
            self.root
        )
    }

    /// Render `unit.profile` and the unit's LTO mode as Cargo does.
    ///
    /// `-C incremental` is left out, because every unit compiles in a fresh
    /// sandbox with no incremental state to reuse. The unstable profile
    /// settings `codegen-backend`, `rustflags`, `trim-paths` and
    /// `hint-mostly-unused` are left out as well, because cargo-schnee does
    /// not pass `-Z` flags through.
    pub(crate) fn new(bcx: &BuildContext<'_, '_>, unit: &Unit, lto: Lto) -> Self {
        let p = &unit.profile;
        let mut args: Vec<String> = Vec::new();
        let mut c = |flag: String| {
            args.push("-C".into());
            args.push(flag);
        };

        if p.opt_level.as_str() != "0" {
            c(format!("opt-level={}", p.opt_level));
        }
        if p.panic != PanicStrategy::Unwind {
            c(format!("panic={}", p.panic));
        }
        match lto {
            Lto::Run(None) => c("lto".into()),
            Lto::Run(Some(s)) => c(format!("lto={s}")),
            Lto::Off => {
                c("lto=off".into());
                c("embed-bitcode=no".into());
            }
            Lto::ObjectAndBitcode => {}
            Lto::OnlyBitcode => c("linker-plugin-lto".into()),
            Lto::OnlyObject => c("embed-bitcode=no".into()),
        }
        if let Some(n) = p.codegen_units {
            c(format!("codegen-units={n}"));
        }
        let debuginfo = p.debuginfo.into_inner().to_string();
        let debug = debuginfo != "0";
        if debug {
            c(format!("debuginfo={debuginfo}"));
            if let Some(split) = p.split_debuginfo
                && bcx
                    .target_data
                    .info(unit.kind)
                    .supports_debuginfo_split(split)
            {
                c(format!("split-debuginfo={split}"));
            }
        }
        // `-C overflow-checks` follows `-C debug-assertions` unless set, so
        // Cargo passes it only where the two differ.
        match (
            p.opt_level.as_str() != "0",
            p.debug_assertions,
            p.overflow_checks,
        ) {
            (true, true, true) => c("debug-assertions=on".into()),
            (true, true, false) => {
                c("debug-assertions=on".into());
                c("overflow-checks=off".into());
            }
            (true, false, true) => c("overflow-checks=on".into()),
            (true, false, false) => {}
            (false, false, overflow) => {
                c("debug-assertions=off".into());
                if overflow {
                    c("overflow-checks=on".into());
                }
            }
            (false, true, false) => c("overflow-checks=off".into()),
            (false, true, true) => {}
        }
        if p.rpath {
            c("rpath".into());
        }
        let strip = p.strip.into_inner();
        if strip != StripInner::None {
            c(format!("strip={strip}"));
        }

        Self {
            rustc_args: args,
            opt_level: p.opt_level.to_string(),
            debug,
            root: match p.root {
                ProfileRoot::Release => "release".into(),
                ProfileRoot::Debug => "debug".into(),
            },
        }
    }
}

/// Decide for every unit whether rustc runs LTO, emits bitcode, or both.
///
/// This is `generate` and `calculate` from `src/cargo/core/compiler/lto.rs`
/// of the `cargo` crate 0.95.0, carried over unchanged. Cargo only computes the map inside `BuildRunner::compile` and
/// `BuildRunner::dry_run`, which consume the runner and drop the map, so a
/// caller that only plans has no public way to read it.
/// The `codegen_flags_match_cargo_*` tests compare the resulting flags with
/// `cargo build -vv` and fail when the two drift apart.
pub(crate) fn lto_modes(bcx: &BuildContext<'_, '_>) -> Result<HashMap<Unit, Lto>> {
    let mut map = HashMap::new();
    for unit in bcx.roots.iter() {
        let root_lto = match unit.profile.lto {
            profiles::Lto::Bool(false) => Lto::OnlyObject,
            profiles::Lto::Off => Lto::Off,
            _ => {
                let crate_types = unit.target.rustc_crate_types();
                if unit.target.for_host() {
                    Lto::OnlyObject
                } else if needs_object(&crate_types) {
                    lto_when_needs_object(&crate_types)
                } else {
                    Lto::OnlyBitcode
                }
            }
        };
        calculate(bcx, &mut map, unit, root_lto)?;
    }
    Ok(map)
}

fn needs_object(crate_types: &[CrateType]) -> bool {
    crate_types.iter().any(|k| k.can_lto() || k.is_dynamic())
}

fn lto_when_needs_object(crate_types: &[CrateType]) -> Lto {
    if crate_types.iter().all(|ct| *ct == CrateType::Dylib) {
        Lto::OnlyObject
    } else {
        Lto::ObjectAndBitcode
    }
}

fn calculate(
    bcx: &BuildContext<'_, '_>,
    map: &mut HashMap<Unit, Lto>,
    unit: &Unit,
    parent_lto: Lto,
) -> Result<()> {
    let crate_types = match unit.mode {
        CompileMode::Test | CompileMode::Doctest => vec![CrateType::Bin],
        _ => unit.target.rustc_crate_types(),
    };
    let all_lto_types = crate_types.iter().all(CrateType::can_lto);
    let lto = if unit.target.for_host() {
        Lto::OnlyObject
    } else if all_lto_types {
        match unit.profile.lto {
            profiles::Lto::Named(s) => Lto::Run(Some(s)),
            profiles::Lto::Off => Lto::Off,
            profiles::Lto::Bool(true) => Lto::Run(None),
            profiles::Lto::Bool(false) => Lto::OnlyObject,
        }
    } else {
        match (parent_lto, needs_object(&crate_types)) {
            (Lto::Run(_), false) => Lto::OnlyBitcode,
            (Lto::Run(_), true) | (Lto::OnlyBitcode, true) => lto_when_needs_object(&crate_types),
            (Lto::Off, _) => Lto::Off,
            (_, false) | (Lto::OnlyObject, true) | (Lto::ObjectAndBitcode, true) => parent_lto,
        }
    };

    // A unit shared between an LTO and a non-LTO consumer needs both object
    // code and bitcode.
    let merged_lto = match map.entry(unit.clone()) {
        Entry::Vacant(v) => *v.insert(lto),
        Entry::Occupied(mut v) => {
            let result = match (lto, v.get()) {
                (Lto::OnlyBitcode, Lto::OnlyBitcode) => Lto::OnlyBitcode,
                (Lto::OnlyObject, Lto::OnlyObject) => Lto::OnlyObject,
                (Lto::Run(s), _) | (_, &Lto::Run(s)) => Lto::Run(s),
                (Lto::Off, _) | (_, Lto::Off) => Lto::Off,
                (Lto::ObjectAndBitcode, _) | (_, Lto::ObjectAndBitcode) => Lto::ObjectAndBitcode,
                (Lto::OnlyObject, Lto::OnlyBitcode) | (Lto::OnlyBitcode, Lto::OnlyObject) => {
                    Lto::ObjectAndBitcode
                }
            };
            if result == *v.get() {
                return Ok(());
            }
            v.insert(result);
            result
        }
    };

    let Some(deps) = bcx.unit_graph.get(unit) else {
        bail!(
            "unit {} is missing from Cargo's unit graph",
            unit.pkg.name()
        );
    };
    for dep in deps {
        calculate(bcx, map, &dep.unit, merged_lto)?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::derivation::build_compile_script;
    use super::super::{NixUnit, TargetConfig, UnitKind, fresh_unit_graph};
    use cargo::util::command_prelude::UserIntent;
    use std::collections::{BTreeMap, HashMap};
    use std::path::Path;
    use std::process::Command;

    /// A workspace with one unit of each kind whose flags Cargo derives
    /// differently: `pmdep` is only a proc-macro dependency, `shared` is
    /// linked by the proc macro and by the binary and carries a
    /// per-package override, `withbuild` has a build script, `app` links
    /// everything with LTO, and `plugin` is a `cdylib` that runs LTO too.
    const FIXTURE: &[(&str, &str)] = &[
        (
            "Cargo.toml",
            r#"[workspace]
members = ["app", "plugin", "pm", "pmdep", "shared", "withbuild"]
resolver = "2"

[profile.release]
lto = "fat"
codegen-units = 4
panic = "abort"
overflow-checks = true

[profile.release.package.shared]
opt-level = 1

[profile.dev.package.withbuild]
opt-level = 2
debug-assertions = false
"#,
        ),
        (
            "pmdep/Cargo.toml",
            "[package]\nname = \"pmdep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("pmdep/src/lib.rs", "pub fn n() -> u32 { 1 }\n"),
        (
            "shared/Cargo.toml",
            "[package]\nname = \"shared\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("shared/src/lib.rs", "pub fn m() -> u32 { 2 }\n"),
        (
            "pm/Cargo.toml",
            r#"[package]
name = "pm"
version = "0.1.0"
edition = "2021"

[lib]
proc-macro = true

[dependencies]
pmdep = { path = "../pmdep" }
shared = { path = "../shared" }
"#,
        ),
        (
            "pm/src/lib.rs",
            r#"use proc_macro::TokenStream;

#[proc_macro]
pub fn three(_: TokenStream) -> TokenStream {
    format!("{}", pmdep::n() + shared::m()).parse().unwrap()
}
"#,
        ),
        (
            "withbuild/Cargo.toml",
            "[package]\nname = \"withbuild\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        ),
        ("withbuild/build.rs", "fn main() {}\n"),
        ("withbuild/src/lib.rs", "pub fn k() -> u32 { 4 }\n"),
        (
            "app/Cargo.toml",
            r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
pm = { path = "../pm" }
shared = { path = "../shared" }
withbuild = { path = "../withbuild" }
"#,
        ),
        (
            "app/src/main.rs",
            "fn main() { println!(\"{}\", pm::three!() + shared::m() + withbuild::k()); }\n",
        ),
        (
            "plugin/Cargo.toml",
            r#"[package]
name = "plugin"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
shared = { path = "../shared" }
"#,
        ),
        (
            "plugin/src/lib.rs",
            "#[no_mangle]\npub extern \"C\" fn plugin() -> u32 { shared::m() }\n",
        ),
    ];

    /// `-C` keys whose values come from the profile or the LTO decision.
    /// `incremental` is absent because cargo-schnee never passes it.
    const CODEGEN_KEYS: &[&str] = &[
        "prefer-dynamic",
        "opt-level",
        "panic",
        "lto",
        "linker-plugin-lto",
        "embed-bitcode",
        "codegen-units",
        "debuginfo",
        "split-debuginfo",
        "debug-assertions",
        "overflow-checks",
        "rpath",
        "strip",
    ];

    /// Rustc invocations grouped by crate name and crate types, each as the
    /// list of its codegen flags. A crate Cargo compiles twice, once for a
    /// proc macro and once for the binary, has two entries.
    type Invocations = BTreeMap<String, Vec<Vec<String>>>;

    fn codegen_flags(tokens: &[&str]) -> Option<(String, Vec<String>)> {
        let mut crate_name = None;
        let mut crate_types = Vec::new();
        let mut flags = Vec::new();
        for pair in tokens.windows(2) {
            match pair[0] {
                "--crate-name" => crate_name = Some(pair[1]),
                "--crate-type" => crate_types.push(pair[1]),
                "-C" => {
                    let key = pair[1].split('=').next().unwrap_or_default();
                    if CODEGEN_KEYS.contains(&key) {
                        flags.push(pair[1].to_string());
                    }
                }
                _ => {}
            }
        }
        crate_types.sort();
        Some((format!("{} {}", crate_name?, crate_types.join(",")), flags))
    }

    /// Build-script runs by package, as `OPT_LEVEL`, `DEBUG` and `PROFILE`.
    type ScriptEnvs = BTreeMap<String, Vec<String>>;

    fn cargo_invocations(dir: &Path, profile: &str) -> (Invocations, ScriptEnvs) {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let home = tempfile::tempdir().unwrap();
        let mut cmd = Command::new(cargo);
        cmd.args(["build", "-vv", "--offline", "--profile", profile])
            .arg("--target-dir")
            .arg(dir.join("target"))
            .current_dir(dir)
            .env("CARGO_HOME", home.path());
        // `CARGO_PROFILE_*` stays, because the in-process planner reads it too
        // and nixpkgs' hooks set `CARGO_PROFILE_RELEASE_STRIP=false`.
        // cargo-schnee ignores `RUSTFLAGS`, so Cargo must not see them.
        for (k, _) in std::env::vars() {
            if k.contains("RUSTFLAGS") {
                cmd.env_remove(k);
            }
        }
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "cargo build failed:\n{stderr}");

        let mut rustc = Invocations::new();
        let mut scripts = ScriptEnvs::new();
        for line in stderr.lines() {
            let Some(cmdline) = line.trim().strip_prefix("Running `") else {
                continue;
            };
            let tokens: Vec<&str> = cmdline.trim_end_matches('`').split_whitespace().collect();
            if let Some((key, flags)) = codegen_flags(&tokens) {
                rustc.entry(key).or_default().push(flags);
            } else if tokens
                .last()
                .is_some_and(|t| t.ends_with("build-script-build"))
            {
                let env = |name: &str| {
                    tokens
                        .iter()
                        .find_map(|t| t.strip_prefix(&format!("{name}=")))
                        .unwrap_or_default()
                        .to_string()
                };
                scripts
                    .entry(env("CARGO_PKG_NAME"))
                    .or_default()
                    .extend(["OPT_LEVEL", "DEBUG", "PROFILE"].map(|n| format!("{n}={}", env(n))));
            }
        }
        (rustc, scripts)
    }

    /// `fresh_unit_graph` sets `CARGO_HOME` and the working directory of the
    /// whole process, so two planners must not overlap. Every test that plans
    /// takes this lock.
    pub(in crate::plan_nix) static PLANNER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn schnee_invocations(dir: &Path, profile: &str) -> (Invocations, ScriptEnvs) {
        let vendor = tempfile::tempdir().unwrap();
        let guard = PLANNER.lock().unwrap_or_else(|e| e.into_inner());
        let (units, _, _) = fresh_unit_graph(
            dir,
            vendor.path(),
            profile,
            &TargetConfig::native(),
            UserIntent::Build,
            &[],
            &[],
            &[],
            false,
            false,
            None,
        )
        .unwrap();
        drop(guard);
        let key_to_idx: HashMap<String, usize> = units
            .iter()
            .enumerate()
            .map(|(i, u)| (u.key.clone(), i))
            .collect();

        let mut rustc = Invocations::new();
        let mut scripts = ScriptEnvs::new();
        for unit in &units {
            if unit.kind == UnitKind::BuildScriptRun {
                scripts.entry(package(unit)).or_default().extend([
                    format!("OPT_LEVEL={}", unit.profile.opt_level),
                    format!("DEBUG={}", unit.profile.debug),
                    format!("PROFILE={}", unit.profile.root),
                ]);
                continue;
            }
            let script = build_compile_script(
                unit,
                &units,
                &key_to_idx,
                &HashMap::new(),
                "rustc",
                "",
                "/sysroot",
                "/coreutils/bin",
                "/cc/bin",
                &TargetConfig::native(),
                &[],
                &[],
                &[],
                "/src",
                &[],
            )
            .unwrap();
            let tokens: Vec<&str> = script.split_whitespace().collect();
            let (key, flags) = codegen_flags(&tokens).unwrap();
            rustc.entry(key).or_default().push(flags);
        }
        (rustc, scripts)
    }

    fn package(unit: &NixUnit) -> String {
        unit.cargo_envs
            .iter()
            .find(|(k, _)| k == "CARGO_PKG_NAME")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    fn sorted(mut invocations: Invocations) -> Invocations {
        invocations.values_mut().for_each(|v| v.sort());
        invocations
    }

    fn assert_flags_match(profile: &str) {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in FIXTURE {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let (cargo_rustc, cargo_scripts) = cargo_invocations(dir.path(), profile);
        let (schnee_rustc, schnee_scripts) = schnee_invocations(dir.path(), profile);
        assert!(
            ["pmdep lib", "app bin", "plugin cdylib"]
                .iter()
                .all(|k| cargo_rustc.contains_key(*k)),
            "cargo -vv output was not parsed: {cargo_rustc:#?}"
        );
        assert_eq!(
            sorted(schnee_rustc),
            sorted(cargo_rustc),
            "profile {profile}"
        );
        assert_eq!(schnee_scripts, cargo_scripts, "profile {profile}");
    }

    #[test]
    fn codegen_flags_match_cargo_release() {
        assert_flags_match("release");
    }

    #[test]
    fn codegen_flags_match_cargo_dev() {
        assert_flags_match("dev");
    }
}
