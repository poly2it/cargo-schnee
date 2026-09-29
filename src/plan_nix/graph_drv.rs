//! The unit graph as a derivation whose `.drv` path cargo-schnee computes
//! without running cargo.
//!
//! The derivation runs `cargo-schnee compute-graph` over a skeleton of the
//! project and over the vendor farm, so the store memoises the graph by
//! the content of its inputs. A warm build finds it realised and reads
//! `graph.json`, and only a miss pays for the cargo bootstrap, inside the
//! sandbox.

use super::derivation::{downstream_placeholder, self_placeholder};
use super::vendor_farm::{BuildTools, PlannedDrv};
use anyhow::Result;

/// The inputs of a unit-graph derivation besides its build tools.
pub(crate) struct GraphInputs<'a> {
    /// The skeleton of the project, see `crate::add_graph_skeleton`.
    pub(crate) skeleton: &'a str,
    pub(crate) vendor_farm_drv: &'a str,
    /// A `cargo-schnee` binary inside the store, which the sandbox can run.
    pub(crate) schnee_bin: &'a str,
    pub(crate) schnee_store: &'a str,
    pub(crate) rustc_store: &'a str,
    /// `compute-graph` flags that select the same graph as this build.
    pub(crate) args: Vec<String>,
    /// The `CARGO_PROFILE_*` variables of the planning process, which
    /// change the profile of every unit.
    pub(crate) profile_env: Vec<(String, String)>,
    /// The contents of the `.cargo/config*` files in the directories above
    /// the project, farthest first. Cargo merges them under the project's
    /// own, which the skeleton carries.
    pub(crate) parent_configs: Vec<String>,
}

/// The `CARGO_PROFILE_*` variables of this process, sorted by name.
pub(crate) fn profile_env() -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CARGO_PROFILE_"))
        .collect();
    vars.sort();
    vars
}

/// The `.cargo/config.toml` or `.cargo/config` of every directory above
/// `project_dir`, farthest first.
pub(crate) fn parent_configs(project_dir: &std::path::Path) -> Vec<String> {
    let mut configs: Vec<String> = project_dir
        .ancestors()
        .skip(1)
        .filter_map(|dir| {
            ["config.toml", "config"]
                .iter()
                .find_map(|f| std::fs::read_to_string(dir.join(".cargo").join(f)).ok())
        })
        .collect();
    configs.reverse();
    configs
}

pub(crate) fn plan_graph_drv(
    name: &str,
    inputs: &GraphInputs,
    tools: &BuildTools,
) -> Result<PlannedDrv> {
    let quoted: Vec<String> = inputs.args.iter().map(|a| shell_quote(a)).collect();
    // Each parent config goes one directory further down, so the project
    // ends up below all of them in the order cargo merges them.
    let mut nest = String::new();
    for i in 0..inputs.parent_configs.len() {
        nest.push_str(&format!(
            "mkdir -p .cargo && printf '%s' \"$parentConfig{i}\" > .cargo/config.toml && mkdir -p d && cd d\n"
        ));
    }
    let script = format!(
        "set -e\n\
         export PATH={rustc}/bin:{coreutils}/bin\n\
         export HOME=\"$TMPDIR\"\n\
         {nest}\
         cp -r {skeleton} workspace\n\
         chmod -R u+w workspace\n\
         cd workspace\n\
         mkdir -p \"$out\"\n\
         {schnee} schnee compute-graph --manifest-path Cargo.toml \
         --vendor-dir \"$vendor\" --output \"$out/graph.json\" {args}\n",
        rustc = inputs.rustc_store,
        coreutils = tools.coreutils_store,
        skeleton = inputs.skeleton,
        schnee = inputs.schnee_bin,
        args = quoted.join(" "),
    );
    let mut env = serde_json::json!({
        "allowSubstitutes": "",
        "out": self_placeholder("out"),
        "preferLocalBuild": "1",
        "vendor": downstream_placeholder(inputs.vendor_farm_drv, "out")?,
    });
    for (key, value) in &inputs.profile_env {
        env[key] = serde_json::Value::String(value.clone());
    }
    for (i, config) in inputs.parent_configs.iter().enumerate() {
        env[format!("parentConfig{i}")] = serde_json::Value::String(config.clone());
    }
    let json = serde_json::json!({
        "name": name,
        "system": tools.system,
        "builder": tools.bash_path,
        "args": ["-c", script],
        "outputs": {"out": {"hashAlgo": "sha256", "method": "nar"}},
        "inputDrvs": {
            inputs.vendor_farm_drv: {"outputs": ["out"], "dynamicOutputs": {}},
        },
        "inputSrcs": [
            tools.bash_store,
            tools.coreutils_store,
            inputs.rustc_store,
            inputs.schnee_store,
            inputs.skeleton,
        ],
        "env": env,
    });
    PlannedDrv::new(name, &json)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_survives_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    fn tools() -> BuildTools {
        BuildTools {
            bash_path: "/nix/store/00000000000000000000000000000000-bash/bin/bash".into(),
            bash_store: "/nix/store/00000000000000000000000000000000-bash".into(),
            coreutils_store: "/nix/store/11111111111111111111111111111111-coreutils".into(),
            tar_store: "/nix/store/22222222222222222222222222222222-gnutar".into(),
            gzip_store: "/nix/store/33333333333333333333333333333333-gzip".into(),
            system: "x86_64-linux".into(),
        }
    }

    fn graph_path(profile_env: Vec<(String, String)>, parent_configs: Vec<String>) -> String {
        let inputs = GraphInputs {
            skeleton: "/nix/store/44444444444444444444444444444444-project-src-skeleton",
            vendor_farm_drv: "/nix/store/55555555555555555555555555555555-vendor.drv",
            schnee_bin: "/nix/store/66666666666666666666666666666666-cargo-schnee/bin/cargo-schnee",
            schnee_store: "/nix/store/66666666666666666666666666666666-cargo-schnee",
            rustc_store: "/nix/store/77777777777777777777777777777777-rust",
            args: vec!["--intent".into(), "build".into()],
            profile_env,
            parent_configs,
        };
        plan_graph_drv("app-build-unit-graph", &inputs, &tools())
            .unwrap()
            .path
    }

    /// The graph carries every unit's profile, so a profile variable must
    /// move the derivation, or a changed profile reuses a stale graph.
    #[test]
    fn profile_variables_move_the_graph_path() {
        let plain = graph_path(vec![], vec![]);
        let opt = graph_path(
            vec![("CARGO_PROFILE_RELEASE_OPT_LEVEL".into(), "2".into())],
            vec![],
        );
        let opt3 = graph_path(
            vec![("CARGO_PROFILE_RELEASE_OPT_LEVEL".into(), "3".into())],
            vec![],
        );
        assert_ne!(plain, opt);
        assert_ne!(opt, opt3);
    }

    #[test]
    fn parent_configs_move_the_graph_path() {
        let plain = graph_path(vec![], vec![]);
        let config = graph_path(vec![], vec!["[profile.release]\nopt-level = 1\n".into()]);
        assert_ne!(plain, config);
    }

    #[test]
    fn parent_configs_are_read_farthest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        let project = inner.join("project");
        for (dir, body) in [(&outer, "outer"), (&inner, "inner"), (&project, "own")] {
            std::fs::create_dir_all(dir.join(".cargo")).unwrap();
            std::fs::write(dir.join(".cargo/config.toml"), body).unwrap();
        }
        let configs = parent_configs(&project);
        let ours: Vec<&str> = configs
            .iter()
            .map(String::as_str)
            .filter(|c| ["outer", "inner", "own"].contains(c))
            .collect();
        assert_eq!(ours, ["outer", "inner"]);
    }
}
