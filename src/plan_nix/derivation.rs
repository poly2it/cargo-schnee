use super::util::{collect_store_paths, shell_quote};
use super::{NixUnit, ProfileConfig, TargetConfig, UnitKind};
use crate::nix_encoding::{extract_hash_part, nix_base32_encode};
use anyhow::{Context, Result};
use log::debug;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Built-in lookup table mapping `links` values to env vars that tell -sys crates
/// to use pkg-config instead of building bundled C code.
const SYS_PKG_CONFIG_ENVS: &[(&str, &str)] = &[
    ("cubeb", "LIBCUBEB_SYS_USE_PKG_CONFIG"),
    ("git2", "LIBGIT2_NO_VENDOR"),
    ("openssl", "OPENSSL_NO_VENDOR"),
    ("sqlite3", "LIBSQLITE3_SYS_USE_PKG_CONFIG"),
    ("ssh2", "LIBSSH2_SYS_USE_PKG_CONFIG"),
    ("z", "LIBZ_SYS_USE_PKG_CONFIG"),
    ("zip", "LIBZIP_SYS_USE_PKG_CONFIG"),
    ("zstd", "ZSTD_SYS_USE_PKG_CONFIG"),
];

/// Build the derivation JSON for a single unit.
#[allow(clippy::too_many_arguments)]
pub(super) fn construct_derivation(
    units: &[NixUnit],
    idx: usize,
    key_to_idx: &HashMap<String, usize>,
    dep_drv_map: &HashMap<String, String>,
    bash_path: &str,
    rustc_path: &str,
    proc_macro_rlib: &str,
    resolved_sysroot: &str,
    mkdir_path: &str,
    coreutils_store: &str,
    cc_bin_dir: &str,
    cc_closure: &[String],
    system: &str,
    rustc_closure: &[String],
    pkg_config_bin: &Option<String>,
    pkg_config_path: &str,
    sys_build_closure: &[String],
    profile: &ProfileConfig,
    target: &TargetConfig,
    cfg_envs: &[(String, String)],
    custom_sys_env: &[(String, String)],
    passthru_envs: &[(String, String)],
) -> Result<serde_json::Value> {
    let unit = &units[idx];
    let script = match unit.kind {
        UnitKind::BuildScriptRun => build_run_script(
            unit,
            units,
            key_to_idx,
            dep_drv_map,
            mkdir_path,
            coreutils_store,
            rustc_path,
            cc_bin_dir,
            pkg_config_bin,
            pkg_config_path,
            profile,
            target,
            cfg_envs,
            custom_sys_env,
            passthru_envs,
        )?,
        _ => {
            let coreutils_bin_dir = format!("{}/bin", coreutils_store);
            build_compile_script(
                unit,
                units,
                key_to_idx,
                dep_drv_map,
                rustc_path,
                proc_macro_rlib,
                resolved_sysroot,
                &coreutils_bin_dir,
                cc_bin_dir,
                profile,
                target,
            )?
        }
    };

    // env
    let mut env = serde_json::Map::new();
    env.insert(
        "out".into(),
        serde_json::Value::String(self_placeholder("out")),
    );
    env.insert(
        "preferLocalBuild".into(),
        serde_json::Value::String("1".into()),
    );
    env.insert(
        "allowSubstitutes".into(),
        serde_json::Value::String("".into()),
    );

    // inputDrvs
    let mut input_drvs = serde_json::Map::new();
    for (_ext_name, dep_key) in &unit.dep_extern {
        if let Some(drv) = dep_drv_map.get(dep_key) {
            input_drvs
                .entry(drv.clone())
                .or_insert_with(|| serde_json::json!({"dynamicOutputs": {}, "outputs": ["out"]}));
        }
    }
    for dep_key in &unit.all_dep_keys {
        if let Some(drv) = dep_drv_map.get(dep_key) {
            input_drvs
                .entry(drv.clone())
                .or_insert_with(|| serde_json::json!({"dynamicOutputs": {}, "outputs": ["out"]}));
        }
    }
    if let Some(ref bs_key) = unit.build_script_dep
        && let Some(drv) = dep_drv_map.get(bs_key)
    {
        input_drvs
            .entry(drv.clone())
            .or_insert_with(|| serde_json::json!({"dynamicOutputs": {}, "outputs": ["out"]}));
    }
    if let Some(ref bs_compile_key) = unit.build_script_compile_key
        && let Some(drv) = dep_drv_map.get(bs_compile_key)
    {
        input_drvs
            .entry(drv.clone())
            .or_insert_with(|| serde_json::json!({"dynamicOutputs": {}, "outputs": ["out"]}));
    }
    // Links deps (other BuildScriptRun units this depends on for DEP_* env vars)
    for (dep_key, _links_name) in &unit.links_dep_keys {
        if let Some(drv) = dep_drv_map.get(dep_key) {
            input_drvs
                .entry(drv.clone())
                .or_insert_with(|| serde_json::json!({"dynamicOutputs": {}, "outputs": ["out"]}));
        }
    }
    // For linking: add all transitive build script run outputs to inputDrvs.
    // Their `cargo:rustc-link-lib` and `cargo:rustc-link-search` directives need
    // to be read at build time and passed to the linker.
    if unit.needs_linker && unit.kind != UnitKind::BuildScriptRun {
        for dep_key in &unit.all_dep_keys {
            if let Some(&dep_idx) = key_to_idx.get(dep_key)
                && let Some(ref bs_key) = units[dep_idx].build_script_dep
                && let Some(drv) = dep_drv_map.get(bs_key)
            {
                input_drvs.entry(drv.clone()).or_insert_with(
                    || serde_json::json!({"dynamicOutputs": {}, "outputs": ["out"]}),
                );
            }
        }
    }

    // inputSrcs — include tool closures and paths referenced by the script
    let mut input_srcs: HashSet<String> = HashSet::new();
    // Rustc closure includes rust-std (needed for sysroot libs like libproc_macro)
    for path in rustc_closure {
        input_srcs.insert(path.clone());
    }
    input_srcs.insert(coreutils_store.to_string());

    if unit.needs_linker || unit.kind == UnitKind::BuildScriptRun {
        for path in cc_closure {
            input_srcs.insert(path.clone());
        }
    }
    // System deps needed by build scripts (pkg-config, openssl-dev, etc.)
    // and by compile units that link against system libraries.
    if unit.kind == UnitKind::BuildScriptRun || unit.needs_linker {
        for path in sys_build_closure {
            input_srcs.insert(path.clone());
        }
    }

    collect_store_paths(&script, &mut input_srcs);

    let mut input_srcs_vec: Vec<String> = input_srcs.into_iter().collect();
    input_srcs_vec.sort();

    Ok(serde_json::json!({
        "name": unit.drv_name,
        "system": system,
        "builder": bash_path,
        "args": ["-c", script],
        "env": env,
        "inputDrvs": input_drvs,
        "inputSrcs": input_srcs_vec,
        "outputs": { "out": { "hashAlgo": "sha256", "method": "nar" } }
    }))
}

/// Build the shell script for a regular compilation or build-script compilation.
#[allow(clippy::too_many_arguments)]
fn build_compile_script(
    unit: &NixUnit,
    units: &[NixUnit],
    key_to_idx: &HashMap<String, usize>,
    dep_drv_map: &HashMap<String, String>,
    rustc_path: &str,
    _proc_macro_rlib: &str,
    resolved_sysroot: &str,
    coreutils_bin_dir: &str,
    cc_bin_dir: &str,
    profile: &ProfileConfig,
    target: &TargetConfig,
) -> Result<String> {
    let mut parts = vec![
        // Source file
        shell_quote(&unit.source_file),
        // --sysroot pointing to the rustc toolchain root. This makes the sysroot
        // explicit rather than relying on rustc deriving it from its binary path,
        // which may not work reliably in Nix sandboxes.
        "--sysroot".into(),
        resolved_sysroot.to_string(),
        // --crate-name
        "--crate-name".into(),
        unit.crate_name.clone(),
        // --edition
        "--edition".into(),
        unit.edition.clone(),
    ];

    // For cross-compilation: target units need --target flag so rustc uses the
    // correct sysroot subdirectory and produces the right binary format.
    // Host units (proc-macros, build scripts) compile without --target.
    if target.is_cross() && !unit.for_host {
        parts.push("--target".into());
        parts.push(target.target_triple.clone());
        // Explicitly tell rustc which linker to use for the target, since
        // the cross-linker is not named `cc`.
        if unit.needs_linker {
            let linker = format!("{}/{}-gcc", cc_bin_dir, target.target_triple);
            parts.push("-C".into());
            parts.push(format!("linker={}", linker));
        }
    }

    // --crate-type
    let is_proc_macro = unit.crate_types.iter().any(|ct| ct == "proc-macro");
    for ct in &unit.crate_types {
        parts.push("--crate-type".into());
        parts.push(ct.clone());
    }

    // proc-macro crates need --extern proc_macro to access the sysroot proc_macro crate,
    // and -C prefer-dynamic (matching cargo behavior)
    if is_proc_macro {
        parts.push("--extern".into());
        parts.push("proc_macro".into());
        parts.push("-C".into());
        parts.push("prefer-dynamic".into());
    }

    // --test for test harness units (test and bench both use CompileMode::Test)
    if unit.kind == UnitKind::TestCompile {
        parts.push("--test".into());
    }

    // --emit (proc-macro crates don't emit metadata, matching cargo)
    parts.push("--emit".into());
    if is_proc_macro {
        parts.push("dep-info,link".into());
    } else {
        parts.push("dep-info,metadata,link".into());
    }

    // --out-dir $out
    parts.push("--out-dir".into());
    parts.push("$out".into());

    // --cfg feature="X"
    for feat in &unit.features {
        parts.push("--cfg".into());
        parts.push(shell_quote(&format!("feature=\"{}\"", feat)));
    }

    // --cap-lints allow for dependency crates (matches cargo behavior)
    if !unit.is_local {
        parts.push("--cap-lints".into());
        parts.push("allow".into());
    }

    // Emit JSON diagnostics with ANSI colors pre-baked by rustc.
    // cargo-schnee parses these and renders via cargo's Shell::print_ansi_stderr().
    parts.push("--error-format=json".into());
    parts.push("--json=diagnostic-rendered-ansi".into());

    // -C extra-filename and -C metadata
    parts.push("-C".into());
    parts.push(format!("extra-filename={}", unit.extra_filename));
    parts.push("-C".into());
    // metadata = extra_filename without leading dash
    parts.push(format!("metadata={}", &unit.extra_filename[1..]));

    // Profile optimization flags
    if profile.opt_level != "0" {
        parts.push("-C".into());
        parts.push(format!("opt-level={}", profile.opt_level));
    }
    if !profile.debug_info {
        parts.push("-C".into());
        parts.push("debuginfo=0".into());
    }

    // --extern deps
    for (extern_name, dep_key) in &unit.dep_extern {
        if let Some(dep_drv) = dep_drv_map.get(dep_key) {
            let dep_unit = &units[key_to_idx[dep_key]];
            let placeholder = downstream_placeholder(dep_drv, "out")?;
            let filename = dep_unit.output_lib_filename();
            parts.push("--extern".into());
            parts.push(format!("{}={}/{}", extern_name, placeholder, filename));
        } else {
            log::warn!(
                "dep_drv_map miss for {}: --extern {} (key {}) will be OMITTED",
                unit.key,
                extern_name,
                dep_key,
            );
        }
    }

    // -L dependency= for transitive deps
    for dep_key in &unit.all_dep_keys {
        if let Some(dep_drv) = dep_drv_map.get(dep_key) {
            let placeholder = downstream_placeholder(dep_drv, "out")?;
            parts.push("-L".into());
            parts.push(format!("dependency={}", placeholder));
        }
    }

    // Build the script
    let mut script = String::new();

    // PATH for linker
    if unit.needs_linker {
        script.push_str(&format!("export PATH={} && ", shell_quote(cc_bin_dir)));
    }

    // Initialize EXTRA_ARGS for build script directives
    script.push_str(r#"EXTRA_ARGS="" && "#);

    // Parse build script output if we depend on one
    if let Some(ref bs_key) = unit.build_script_dep
        && let Some(bs_drv) = dep_drv_map.get(bs_key)
    {
        let bs_placeholder = downstream_placeholder(bs_drv, "out")?;
        // Read cargo: directives from own build script output
        script.push_str(&format!(
            r#"export OUT_DIR={ph}/out_dir && if [ -f {ph}/output ]; then while IFS= read -r line; do case "$line" in cargo:rustc-cfg=*) EXTRA_ARGS="$EXTRA_ARGS --cfg ${{line#cargo:rustc-cfg=}}" ;; cargo:rustc-env=*) kv="${{line#cargo:rustc-env=}}"; export "${{kv%%=*}}=${{kv#*=}}" ;; cargo:rustc-link-lib=*) EXTRA_ARGS="$EXTRA_ARGS -l ${{line#cargo:rustc-link-lib=}}" ;; cargo:rustc-link-search=*) EXTRA_ARGS="$EXTRA_ARGS -L ${{line#cargo:rustc-link-search=}}" ;; esac; done < {ph}/output; fi && "#,
            ph = bs_placeholder,
        ));
    }

    // For linking: read cargo:rustc-link-lib and cargo:rustc-link-search from
    // ALL transitive dependencies' build script outputs. Cargo propagates these
    // to the final linker invocation.
    if unit.needs_linker {
        for dep_key in &unit.all_dep_keys {
            if let Some(&dep_idx) = key_to_idx.get(dep_key)
                && let Some(ref bs_key) = units[dep_idx].build_script_dep
            {
                // Skip own build script (already handled above)
                if unit.build_script_dep.as_ref() == Some(bs_key) {
                    continue;
                }
                if let Some(bs_drv) = dep_drv_map.get(bs_key) {
                    let bs_placeholder = downstream_placeholder(bs_drv, "out")?;
                    script.push_str(&format!(
                        r#"if [ -f {ph}/output ]; then while IFS= read -r line; do case "$line" in cargo:rustc-link-lib=*) EXTRA_ARGS="$EXTRA_ARGS -l ${{line#cargo:rustc-link-lib=}}" ;; cargo:rustc-link-search=*) EXTRA_ARGS="$EXTRA_ARGS -L ${{line#cargo:rustc-link-search=}}" ;; esac; done < {ph}/output; fi && "#,
                        ph = bs_placeholder,
                    ));
                }
            }
        }
    }

    // Set cargo env vars
    for (k, v) in &unit.cargo_envs {
        script.push_str(&format!("export {}={} && ", k, shell_quote(v)));
    }
    script.push_str(&format!(
        "export CARGO_MANIFEST_DIR={} && ",
        shell_quote(&unit.manifest_dir)
    ));

    let mkdir_path = format!("{}/mkdir", coreutils_bin_dir);
    let cat_path = format!("{}/cat", coreutils_bin_dir);
    script.push_str(&format!(
        "{} -p $out && {} {}",
        shell_quote(&mkdir_path),
        shell_quote(rustc_path),
        parts.join(" "),
    ));

    // Append $EXTRA_ARGS (own + transitive build script link directives)
    // Capture stderr to $out/diagnostics for replay on cached builds,
    // then replay to stderr for live display. Preserve rustc exit code.
    script.push_str(&format!(
        " $EXTRA_ARGS 2>$out/diagnostics; __rs=$?; {} $out/diagnostics >&2; exit $__rs",
        shell_quote(&cat_path),
    ));

    Ok(script)
}

/// Build the shell script for running a build script.
#[allow(clippy::too_many_arguments)]
fn build_run_script(
    unit: &NixUnit,
    units: &[NixUnit],
    key_to_idx: &HashMap<String, usize>,
    dep_drv_map: &HashMap<String, String>,
    mkdir_path: &str,
    coreutils_store: &str,
    rustc_path: &str,
    cc_bin_dir: &str,
    pkg_config_bin: &Option<String>,
    pkg_config_path: &str,
    profile: &ProfileConfig,
    target: &TargetConfig,
    cfg_envs: &[(String, String)],
    custom_sys_env: &[(String, String)],
    passthru_envs: &[(String, String)],
) -> Result<String> {
    // The build script compile derivation provides the binary
    let bs_compile_key = unit
        .build_script_compile_key
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("BuildScriptRun {} has no compile key", unit.key))?;
    let bs_compile_drv = dep_drv_map.get(bs_compile_key).ok_or_else(|| {
        anyhow::anyhow!("Build script compile drv not found for {}", bs_compile_key)
    })?;
    let bs_compile_unit = &units[key_to_idx[bs_compile_key]];
    let bs_binary = bs_compile_unit.output_lib_filename();
    let bs_placeholder = downstream_placeholder(bs_compile_drv, "out")?;

    let mut script = String::new();
    script.push_str(&format!(
        "{} -p $out $out/out_dir && ",
        shell_quote(mkdir_path)
    ));

    // Set PATH so build scripts can find cc, ar, coreutils (tr, etc.), pkg-config
    let coreutils_bin = format!("{}/bin", coreutils_store);
    let mut path_dirs = vec![cc_bin_dir.to_string(), coreutils_bin];
    if let Some(pkg_config) = pkg_config_bin
        && let Some(dir) = PathBuf::from(pkg_config).parent()
    {
        path_dirs.push(dir.to_string_lossy().to_string());
    }
    script.push_str(&format!("export PATH={} && ", path_dirs.join(":")));

    // Set PKG_CONFIG_PATH for -sys crate build scripts
    if !pkg_config_path.is_empty() {
        script.push_str(&format!(
            "export PKG_CONFIG_PATH={} && ",
            shell_quote(pkg_config_path)
        ));
    }

    // Tell -sys crates to use pkg-config instead of building bundled C code,
    // but only for the specific build script that needs it.
    if let Some(links) = unit.links.as_deref()
        && !pkg_config_path.is_empty()
    {
        // Check custom overrides first, then built-in table
        let env_var = custom_sys_env
            .iter()
            .find(|(k, _)| k == links)
            .map(|(_, v)| v.as_str())
            .or_else(|| {
                SYS_PKG_CONFIG_ENVS
                    .iter()
                    .find(|(k, _)| *k == links)
                    .map(|(_, v)| *v)
            });
        if let Some(var) = env_var {
            script.push_str(&format!("export {}=1 && ", var));
        }
    }

    // Set standard build script env vars
    script.push_str("export OUT_DIR=$out/out_dir && ");
    script.push_str(&format!("export RUSTC={} && ", shell_quote(rustc_path)));
    script.push_str(&format!("export HOST={} && ", target.host_triple));
    script.push_str(&format!("export TARGET={} && ", target.target_triple));
    script.push_str("export NUM_JOBS=1 && ");
    script.push_str(&format!("export OPT_LEVEL={} && ", profile.opt_level));
    script.push_str(&format!(
        "export DEBUG={} && ",
        if profile.debug_info { "true" } else { "false" }
    ));
    script.push_str(&format!("export PROFILE={} && ", profile.name));

    // Cargo target cfg vars (extracted from rustc --print cfg via cargo internals)
    for (key, val) in cfg_envs {
        script.push_str(&format!("export {}={} && ", key, shell_quote(val)));
    }

    // Package env vars
    for (k, v) in &unit.cargo_envs {
        script.push_str(&format!("export {}={} && ", k, shell_quote(v)));
    }
    script.push_str(&format!(
        "export CARGO_MANIFEST_DIR={} && ",
        shell_quote(&unit.manifest_dir)
    ));
    script.push_str(&format!(
        "export CARGO_MANIFEST_PATH={}/Cargo.toml && ",
        shell_quote(&unit.manifest_dir)
    ));
    if let Some(links) = unit.links.as_deref() {
        script.push_str(&format!("export CARGO_MANIFEST_LINKS={} && ", links));
    }

    // CARGO_FEATURE_<NAME>=1 for each enabled feature
    for feat in &unit.features {
        let feat_env = feat.to_uppercase().replace('-', "_");
        script.push_str(&format!("export CARGO_FEATURE_{}=1 && ", feat_env));
    }

    // Passthrough env vars forwarded from the outer Nix derivation
    for (k, v) in passthru_envs {
        script.push_str(&format!("export {}={} && ", k, shell_quote(v)));
    }

    // DEP_<LINKS>_<KEY> env vars from dependency build scripts
    for (dep_key, links_name) in &unit.links_dep_keys {
        if let Some(dep_drv) = dep_drv_map.get(dep_key) {
            let dep_placeholder = downstream_placeholder(dep_drv, "out")?;
            let links_upper = links_name.to_uppercase().replace('-', "_");
            script.push_str(&format!(
                r#"if [ -f {ph}/output ]; then while IFS= read -r line; do case "$line" in cargo:*=*) key="${{line#cargo:}}"; key="${{key%%=*}}"; val="${{line#*=}}"; case "$key" in rustc-cfg|rustc-env|rustc-link-lib|rustc-link-search|rerun-if-changed|rerun-if-env-changed|warning) ;; *) export "DEP_{links}_$(echo "$key" | tr '[:lower:]-' '[:upper:]_')=$val" ;; esac ;; esac; done < {ph}/output; fi && "#,
                ph = dep_placeholder,
                links = links_upper,
            ));
        }
    }

    // Create a writable copy of the manifest dir so build scripts that read
    // files relative to CWD (cargo convention) AND scripts that write temp
    // files relative to CWD (e.g. embedded DB engines) both work.
    script.push_str("export HOME=$TMPDIR && ");
    script.push_str("_bs_workdir=$TMPDIR/workdir && ");
    script.push_str("cp -r --no-preserve=mode $CARGO_MANIFEST_DIR/. $_bs_workdir && ");
    script.push_str(&format!(
        "cd $_bs_workdir && {}/{} > $out/output",
        bs_placeholder, bs_binary,
    ));

    Ok(script)
}

pub(super) fn nix_store_closure(store_path: &str) -> Result<Vec<String>> {
    let output = Command::new("nix-store")
        .arg("-qR")
        .arg(store_path)
        .output()
        .context("Failed to run nix-store -qR")?;
    if !output.status.success() {
        anyhow::bail!(
            "nix-store -qR failed for {}: {}",
            store_path,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(|s| s.to_string())
        .collect())
}

pub(super) fn nix_derivation_add(json: &serde_json::Value) -> Result<String> {
    let json_str = serde_json::to_string(json)?;
    debug!("nix derivation add input: {}", json_str);
    let mut child = Command::new("nix")
        .args([
            "derivation",
            "add",
            "--extra-experimental-features",
            "nix-command ca-derivations",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn nix derivation add")?;
    {
        use std::io::Write;
        child
            .stdin
            .take()
            .context("stdin not piped")?
            .write_all(json_str.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "nix derivation add failed: {}\nJSON: {}",
            String::from_utf8_lossy(&output.stderr),
            json_str
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

pub(super) fn downstream_placeholder(drv_store_path: &str, output_name: &str) -> Result<String> {
    let hash_part = extract_hash_part(drv_store_path)?;
    let basename = drv_store_path
        .strip_prefix("/nix/store/")
        .unwrap_or(drv_store_path);
    anyhow::ensure!(
        basename.len() > 33,
        "malformed drv store path (too short): {}",
        drv_store_path
    );
    let drv_name = &basename[33..];
    let drv_name = drv_name.strip_suffix(".drv").unwrap_or(drv_name);
    let output_path_name = if output_name == "out" {
        drv_name.to_string()
    } else {
        format!("{}-{}", drv_name, output_name)
    };
    let input = format!("nix-upstream-output:{}:{}", hash_part, output_path_name);
    let digest = Sha256::digest(input.as_bytes());
    Ok(format!("/{}", nix_base32_encode(&digest)))
}

pub(super) fn self_placeholder(output_name: &str) -> String {
    let input = format!("nix-output:{}", output_name);
    let digest = Sha256::digest(input.as_bytes());
    format!("/{}", nix_base32_encode(&digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_placeholder_format() {
        let ph = self_placeholder("out");
        assert_eq!(ph.len(), 53);
        assert!(ph.starts_with('/'));
    }

    #[test]
    fn self_placeholder_deterministic() {
        assert_eq!(self_placeholder("out"), self_placeholder("out"));
    }

    #[test]
    fn self_placeholder_differs_by_name() {
        assert_ne!(self_placeholder("out"), self_placeholder("dev"));
    }

    #[test]
    fn downstream_placeholder_format() {
        let drv_path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo.drv";
        let ph = downstream_placeholder(drv_path, "out").unwrap();
        assert_eq!(ph.len(), 53);
        assert!(ph.starts_with('/'));
    }

    #[test]
    fn downstream_placeholder_differs_by_output() {
        let drv_path = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo.drv";
        let out = downstream_placeholder(drv_path, "out").unwrap();
        let dev = downstream_placeholder(drv_path, "dev").unwrap();
        assert_ne!(out, dev);
    }

    #[test]
    fn downstream_placeholder_short_path() {
        assert!(downstream_placeholder("/nix/store/short", "out").is_err());
    }
}
