use super::util::{collect_store_paths, shell_quote};
use super::{NixUnit, ProfileConfig, TargetConfig, UnitKind};
use crate::nix_encoding::{extract_hash_part, hex_lower, nix_base32_encode};
use anyhow::{Context, Result};
use tracing::debug;
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
    // Store root containing `bash_path`'s binary.  Must be in `inputSrcs`
    // so the sandbox bind-mounts the builder; see `util::which_bash`.
    bash_store: &str,
    rustc_path: &str,
    rustdoc_path: &str,
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
    host_cfg_envs: &[(String, String)],
    custom_sys_env: &[(String, String)],
    passthru_envs: &[(String, String)],
    vendor_store: &str,
    win_sdk_lib_dirs: &[String],
    win_sdk_closure: &[String],
    src_store: &str,
    document_private_items: bool,
    passthru_closure: &[String],
    // When `Some`, swap rustc for clippy-driver on local (workspace) compile
    // units.  Dep units keep using rustc so their per-unit derivations stay
    // byte-identical to a regular check / build run.
    clippy_path: Option<&str>,
    // clippy-driver's nix store closure.  Only added to inputSrcs of units
    // that actually use clippy_path so dep units are unaffected.
    clippy_closure: &[String],
    // Lint args forwarded to clippy-driver after the rustc command line.
    // Only applied to units that actually run clippy so dep-unit derivation
    // hashes stay stable when the caller toggles deny-warnings on or off.
    clippy_lint_args: &[String],
    // `--remap-path-prefix` rules to inject into every compile unit's rustc
    // command line.  Each pair is `(src_relative, replacement)` where
    // `src_relative` is interpreted relative to `src_store` — empty string
    // remaps the project-src root itself.  Sorted shortest-first inside
    // `build_compile_script` so rustc's "last matching wins" rule resolves
    // longer (more specific) entries on top of shorter ones.
    path_prefix_remaps: &[(String, String)],
    // Store paths of caller-supplied setup scripts, in rule order, sourced
    // into the unit's sandbox right before the driver / build-script
    // invocation.  Empty for units no rule matches, keeping those units'
    // derivations byte-identical to a run without rules.  The paths land
    // in `inputSrcs` via the `collect_store_paths` scan of the script
    // text, so the scripts and their reference closures are mounted with
    // no extra plumbing.
    setup_scripts: &[String],
) -> Result<serde_json::Value> {
    let unit = &units[idx];
    let coreutils_bin_dir = format!("{}/bin", coreutils_store);

    // Decide whether this unit should be linted.  Doc and BuildScriptRun
    // are excluded — Doc runs rustdoc, BuildScriptRun executes a binary.
    // Only local (workspace) units swap; deps keep their cached rustc drvs.
    let use_clippy = clippy_path.is_some()
        && unit.is_local
        && !matches!(unit.kind, UnitKind::Doc | UnitKind::BuildScriptRun);
    let effective_rustc = if use_clippy {
        clippy_path.unwrap()
    } else {
        rustc_path
    };

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
            host_cfg_envs,
            custom_sys_env,
            passthru_envs,
            src_store,
            setup_scripts,
        )?,
        UnitKind::Doc => build_doc_script(
            unit,
            units,
            key_to_idx,
            dep_drv_map,
            rustdoc_path,
            resolved_sysroot,
            &coreutils_bin_dir,
            document_private_items,
            path_prefix_remaps,
            src_store,
            setup_scripts,
        )?,
        _ => build_compile_script(
            unit,
            units,
            key_to_idx,
            dep_drv_map,
            effective_rustc,
            proc_macro_rlib,
            resolved_sysroot,
            &coreutils_bin_dir,
            cc_bin_dir,
            profile,
            target,
            win_sdk_lib_dirs,
            if use_clippy { clippy_lint_args } else { &[] },
            path_prefix_remaps,
            src_store,
            setup_scripts,
        )?,
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
    // clippy-driver's closure is added only to local units that actually run
    // clippy.  Adding it unconditionally would invalidate dep unit caches.
    if use_clippy {
        for path in clippy_closure {
            input_srcs.insert(path.clone());
        }
    }
    input_srcs.insert(coreutils_store.to_string());
    // The builder shells out via `bash_path`; its containing store root
    // must be bind-mounted into the sandbox or the build fails before any
    // user code runs.
    input_srcs.insert(bash_store.to_string());

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
    // Windows SDK closure for MSVC linking
    if unit.needs_linker && !unit.for_host && !win_sdk_closure.is_empty() {
        for path in win_sdk_closure {
            input_srcs.insert(path.clone());
        }
    }

    // Build scripts may reference vendor crate source files at runtime
    // (e.g. via env!("CARGO_MANIFEST_DIR") baked into the compiled binary).
    // Linking units also need it: build scripts emit cargo:rustc-link-search
    // paths pointing into the vendor store (e.g. pre-built .lib files).
    if (unit.kind == UnitKind::BuildScriptRun || unit.needs_linker) && !vendor_store.is_empty() {
        input_srcs.insert(vendor_store.to_string());
    }
    // passthruEnv values may reference store paths whose closures must be
    // available in build-script-run sandboxes (e.g. LIBCLANG_PATH).
    if unit.kind == UnitKind::BuildScriptRun {
        for path in passthru_closure {
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

/// Build the `--remap-path-prefix` argument tokens for a unit.
///
/// `path_prefix_remaps` are `(src_relative, replacement)` pairs expressed
/// relative to the *project-src* root, so callers need not know the build's
/// content-addressed hash. `src_store` is the unit's actual source store: the
/// project-src store for non-sliced units, or a flat per-crate
/// `<hash>-<member>` store for sliced ones (`assign_per_crate_src_stores`).
///
/// rustc resolves overlapping remaps "last matching wins", so emit shortest
/// `src_relative` first and let longer (more specific) entries override.
///
/// `sliced_crate_rel` re-roots the project-src-root remap for a sliced local
/// crate. Without it, `--remap-path-prefix <crate_store>=<replacement>`
/// collapses `<crate_store>/src/x` to `<replacement>/src/x`, dropping the
/// `<member>/` directory and colliding every crate's `src/lib.rs`. With it the
/// root remap targets `<replacement>/<crate_rel>` so the crate keeps its real
/// workspace path. Only the root remap (`src_relative == ""`) is adjusted; a
/// non-root remap names a subpath that, for a sliced crate, refers to an
/// external source not under this crate store and simply will not match.
fn remap_args(
    path_prefix_remaps: &[(String, String)],
    src_store: &str,
    sliced_crate_rel: Option<&str>,
) -> Vec<String> {
    let mut sorted: Vec<&(String, String)> = path_prefix_remaps.iter().collect();
    sorted.sort_by_key(|(src_relative, _)| src_relative.len());
    let mut out = Vec::new();
    for (src_relative, replacement) in sorted {
        let from = if src_relative.is_empty() {
            src_store.to_string()
        } else {
            format!("{}/{}", src_store, src_relative)
        };
        let to = match sliced_crate_rel {
            Some(rel) if src_relative.is_empty() => format!("{}/{}", replacement, rel),
            _ => replacement.clone(),
        };
        out.push("--remap-path-prefix".into());
        out.push(shell_quote(&format!("{}={}", from, to)));
    }
    out
}

/// Shell fragment sourcing each caller-supplied setup script in rule
/// order, with `SCHNEE_AUX_DIR` — the unit's auxiliary output channel —
/// exported first.  Callers place it after the cargo env exports and the
/// `mkdir` that creates `$out`, immediately before the driver invocation,
/// so a script's exports persist into the driver and an EXIT trap it sets
/// fires after the work completes.  Empty when no rule matched, keeping
/// non-matching units byte-identical to a run without rules.
fn setup_source_fragment(setup_scripts: &[String]) -> String {
    if setup_scripts.is_empty() {
        return String::new();
    }
    let mut s = String::from("export SCHNEE_AUX_DIR=$out/schnee-aux && ");
    for script in setup_scripts {
        s.push_str(&format!(". {} && ", shell_quote(script)));
    }
    s
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
    win_sdk_lib_dirs: &[String],
    // Extra rustc / clippy-driver flags appended after every other
    // arg.  Used to forward post-`--` clippy lint flags such as
    // `--deny warnings`; empty for normal compile units.
    extra_rustc_args: &[String],
    // `--remap-path-prefix` rules.  See `construct_derivation` doc.
    path_prefix_remaps: &[(String, String)],
    // Project-src store path; remaps with `src_relative = ""` rewrite this
    // root, longer entries rewrite subdirectories.
    src_store: &str,
    // Matched setup script store paths.  See `construct_derivation` doc.
    setup_scripts: &[String],
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
        // Explicitly tell rustc which linker to use for the target.
        if unit.needs_linker {
            if target.is_msvc() {
                // MSVC: use lld-link (found in cc_bin_dir via find_cross_linker)
                let linker = format!("{}/lld-link", cc_bin_dir);
                parts.push("-C".into());
                parts.push(format!("linker={}", linker));
                // Statically link the MSVC CRT — the runtime DLLs
                // (vcruntime140.dll, ucrtbase.dll) aren't available when
                // cross-compiling from Linux.
                parts.push("-C".into());
                parts.push("target-feature=+crt-static".into());
                // Add Windows SDK library search paths
                for lib_dir in win_sdk_lib_dirs {
                    parts.push("-L".into());
                    parts.push(format!("native={}", lib_dir));
                }
            } else {
                // GNU: use {triple}-gcc
                let linker = format!("{}/{}-gcc", cc_bin_dir, target.target_triple);
                parts.push("-C".into());
                parts.push(format!("linker={}", linker));
            }
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

    // --test for test harness units (test and bench both use
    // CompileMode::Test) AND for any Check unit pulled in via
    // `--all-targets` whose mode is `CompileMode::Check { test: true }`
    // — integration tests need rustc to synthesise a `main` and the
    // harness even in check/clippy intent.
    if unit.kind == UnitKind::TestCompile || unit.compile_test {
        parts.push("--test".into());
    }

    // --emit: check units emit metadata only (no codegen/link),
    // proc-macro crates don't emit metadata (matching cargo).
    parts.push("--emit".into());
    if unit.kind == UnitKind::Check {
        parts.push("dep-info,metadata".into());
    } else if is_proc_macro {
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
            tracing::warn!(
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

    // `--remap-path-prefix`: rewrite source paths in diagnostics, debug
    // info, and macro expansions.  Each `(src_relative, replacement)` is
    // interpreted relative to `src_store` so callers don't have to know the
    // per-build content-addressed hash; empty `src_relative` rewrites the
    // project-src root itself.  rustc resolves multiple remaps with
    // "last matching wins" — sort shortest-first so longer (more specific)
    // entries override shorter ones for paths that match both.
    parts.extend(remap_args(
        path_prefix_remaps,
        src_store,
        unit.sliced_crate_rel.as_deref(),
    ));

    // Caller-supplied flags forwarded to clippy-driver (or rustc).  For
    // clippy units this is e.g. `["--deny", "warnings"]` from
    // `cargo schnee clippy -- --deny warnings`; empty for regular
    // compile units.  Appended last so the deny level applies on top of
    // any allow level set by earlier flags.
    for arg in extra_rustc_args {
        parts.push(shell_quote(arg));
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
    // For TestCompile units, use a deterministic /tmp symlink as
    // CARGO_MANIFEST_DIR. At compile time the symlink points to the store
    // path so proc macros (e.g. sqlx::migrate!) can read files. At test
    // runtime the same path is re-symlinked to the writable project dir,
    // so both env!("CARGO_MANIFEST_DIR") and std::env::var() resolve to
    // a readable+writable location.
    let tmp_manifest_path;
    let manifest_dir_for_compile =
        if unit.kind == UnitKind::TestCompile && !unit.original_manifest_dir.is_empty() {
            let hash = {
                let mut hasher = Sha256::new();
                hasher.update(unit.original_manifest_dir.as_bytes());
                hex_lower(&hasher.finalize()[..8])
            };
            tmp_manifest_path = format!("/tmp/_schnee_md_{}", hash);
            let ln_path = format!("{}/ln", coreutils_bin_dir);
            script.push_str(&format!(
                "{} -sfn {} {} && ",
                shell_quote(&ln_path),
                shell_quote(&unit.manifest_dir),
                shell_quote(&tmp_manifest_path),
            ));
            &tmp_manifest_path
        } else {
            &unit.manifest_dir
        };
    script.push_str(&format!(
        "export CARGO_MANIFEST_DIR={} && ",
        shell_quote(manifest_dir_for_compile)
    ));

    let mkdir_path = format!("{}/mkdir", coreutils_bin_dir);
    let cat_path = format!("{}/cat", coreutils_bin_dir);
    // The mkdir stays ahead of the setup hook so `$out` (and thereby
    // SCHNEE_AUX_DIR's parent) exists before the first script sources.
    script.push_str(&format!("{} -p $out && ", shell_quote(&mkdir_path)));
    script.push_str(&setup_source_fragment(setup_scripts));
    script.push_str(&format!("{} {}", shell_quote(rustc_path), parts.join(" ")));

    // Append $EXTRA_ARGS (own + transitive build script link directives)
    // Capture stderr to $out/diagnostics for replay on cached builds,
    // then replay to stderr for live display. Preserve rustc exit code.
    script.push_str(&format!(
        " $EXTRA_ARGS 2>$out/diagnostics; __rs=$?; {} $out/diagnostics >&2; exit $__rs",
        shell_quote(&cat_path),
    ));

    Ok(script)
}

/// Build the shell script for a rustdoc documentation derivation.
#[allow(clippy::too_many_arguments)]
fn build_doc_script(
    unit: &NixUnit,
    units: &[NixUnit],
    key_to_idx: &HashMap<String, usize>,
    dep_drv_map: &HashMap<String, String>,
    rustdoc_path: &str,
    resolved_sysroot: &str,
    coreutils_bin_dir: &str,
    document_private_items: bool,
    path_prefix_remaps: &[(String, String)],
    src_store: &str,
    // Matched setup script store paths.  See `construct_derivation` doc.
    setup_scripts: &[String],
) -> Result<String> {
    let mut parts = vec![
        // Source file
        shell_quote(&unit.source_file),
        // --sysroot
        "--sysroot".into(),
        resolved_sysroot.to_string(),
        // --crate-name
        "--crate-name".into(),
        unit.crate_name.clone(),
        // --edition
        "--edition".into(),
        unit.edition.clone(),
    ];

    // --crate-type
    for ct in &unit.crate_types {
        parts.push("--crate-type".into());
        parts.push(ct.clone());
    }

    // --output: rustdoc writes HTML to this directory
    parts.push("--output".into());
    parts.push("$out/doc".into());

    // --cfg feature="X"
    for feat in &unit.features {
        parts.push("--cfg".into());
        parts.push(shell_quote(&format!("feature=\"{}\"", feat)));
    }

    // --cap-lints allow for dependency crates
    if !unit.is_local {
        parts.push("--cap-lints".into());
        parts.push("allow".into());
    }

    // JSON diagnostics
    parts.push("--error-format=json".into());
    parts.push("--json=diagnostic-rendered-ansi".into());

    // --document-private-items if requested
    if document_private_items && unit.is_local {
        parts.push("--document-private-items".into());
    }

    // -C metadata (rustdoc uses this for cross-crate link stability)
    parts.push("-C".into());
    parts.push(format!("metadata={}", &unit.extra_filename[1..]));

    // `--remap-path-prefix`: rewrite source paths in rustdoc diagnostics so
    // they match the repo, same as the compile path. Without this, doc lints
    // surface raw `<store>/...` paths no downstream mapper can resolve.
    //
    // Unlike rustc, where `--remap-path-prefix` is stable, rustdoc gates the
    // flag behind `-Z unstable-options`. The pinned toolchain reports as
    // stable, so `RUSTC_BOOTSTRAP` is exported below to let rustdoc accept the
    // unstable flag. Only emit the gate when there are remaps to apply.
    let remap = remap_args(
        path_prefix_remaps,
        src_store,
        unit.sliced_crate_rel.as_deref(),
    );
    let needs_unstable_options = !remap.is_empty();
    if needs_unstable_options {
        parts.push("-Z".into());
        parts.push("unstable-options".into());
    }
    parts.extend(remap);

    // --extern deps — point to .rmeta/.rlib from dependency compile outputs
    for (extern_name, dep_key) in &unit.dep_extern {
        if let Some(dep_drv) = dep_drv_map.get(dep_key) {
            let dep_unit = &units[key_to_idx[dep_key]];
            let placeholder = downstream_placeholder(dep_drv, "out")?;
            let filename = dep_unit.output_lib_filename();
            parts.push("--extern".into());
            parts.push(format!("{}={}/{}", extern_name, placeholder, filename));
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

    // Initialize EXTRA_ARGS for build script directives
    script.push_str(r#"EXTRA_ARGS="" && "#);

    // Let the stable-reporting toolchain accept the `-Z unstable-options`
    // gate that rustdoc requires for `--remap-path-prefix`.
    if needs_unstable_options {
        script.push_str("export RUSTC_BOOTSTRAP=1 && ");
    }

    // Parse build script output if we depend on one
    if let Some(ref bs_key) = unit.build_script_dep
        && let Some(bs_drv) = dep_drv_map.get(bs_key)
    {
        let bs_placeholder = downstream_placeholder(bs_drv, "out")?;
        script.push_str(&format!(
            r#"export OUT_DIR={ph}/out_dir && if [ -f {ph}/output ]; then while IFS= read -r line; do case "$line" in cargo:rustc-cfg=*) EXTRA_ARGS="$EXTRA_ARGS --cfg ${{line#cargo:rustc-cfg=}}" ;; cargo:rustc-env=*) kv="${{line#cargo:rustc-env=}}"; export "${{kv%%=*}}=${{kv#*=}}" ;; esac; done < {ph}/output; fi && "#,
            ph = bs_placeholder,
        ));
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
    // The mkdir stays ahead of the setup hook so `$out` (and thereby
    // SCHNEE_AUX_DIR's parent) exists before the first script sources.
    script.push_str(&format!("{} -p $out/doc && ", shell_quote(&mkdir_path)));
    script.push_str(&setup_source_fragment(setup_scripts));
    script.push_str(&format!(
        "{} {}",
        shell_quote(rustdoc_path),
        parts.join(" "),
    ));

    // Append $EXTRA_ARGS and capture diagnostics
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
    host_cfg_envs: &[(String, String)],
    custom_sys_env: &[(String, String)],
    passthru_envs: &[(String, String)],
    src_store: &str,
    // Matched setup script store paths.  See `construct_derivation` doc.
    setup_scripts: &[String],
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
    // In cross builds, only host build scripts (for_host) should use the host
    // pkg-config — target build scripts need target-specific libraries.
    if let Some(links) = unit.links.as_deref()
        && !pkg_config_path.is_empty()
        && (unit.for_host || !target.is_cross())
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

    // Set standard build script env vars.
    // Host-compiled crates (proc-macros and their deps) see TARGET == HOST,
    // matching cargo's behavior during cross-compilation.
    let effective_target = if unit.for_host {
        &target.host_triple
    } else {
        &target.target_triple
    };
    script.push_str("export OUT_DIR=$out/out_dir && ");
    script.push_str(&format!("export RUSTC={} && ", shell_quote(rustc_path)));
    script.push_str(&format!("export HOST={} && ", target.host_triple));
    script.push_str(&format!("export TARGET={} && ", effective_target));
    script.push_str("export NUM_JOBS=1 && ");
    script.push_str(&format!("export OPT_LEVEL={} && ", profile.opt_level));
    script.push_str(&format!(
        "export DEBUG={} && ",
        if profile.debug_info { "true" } else { "false" }
    ));
    script.push_str(&format!("export PROFILE={} && ", profile.name));

    // Cargo target cfg vars (extracted from rustc --print cfg via cargo internals).
    // Host-compiled crates use the host's cfg values, not the cross target's.
    let effective_cfg_envs = if unit.for_host {
        host_cfg_envs
    } else {
        cfg_envs
    };
    for (key, val) in effective_cfg_envs {
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
    // Remember the original (Nix store) manifest dir so we can rewrite
    // workdir paths back to it in the build script output.
    script.push_str("_orig_manifest_dir=$CARGO_MANIFEST_DIR && ");

    // Build scripts access files relative to CARGO_MANIFEST_DIR. For nested
    // crates (manifest_dir = src_store + "/my-crate"), the workdir must
    // mirror the workspace layout so that relative paths to sibling
    // directories like "../spec/" resolve correctly. We copy the full
    // workspace source tree into $TMPDIR/workdir/ and set the workdir to
    // the crate's subdirectory within it.
    //
    // When extra-includes reference files outside the project directory,
    // they are mapped under .parent/ in the source store. .parent/ content
    // is moved to $TMPDIR/ so that paths traversing above the workspace
    // root still resolve.  The dot-prefix prevents Cargo's member globs
    // (e.g. members = ["*"]) from treating it as a workspace member.
    let has_parent = PathBuf::from(src_store).join(".parent").is_dir();
    let crate_rel = unit
        .manifest_dir
        .strip_prefix(src_store)
        .map(|s| s.trim_start_matches('/'))
        .filter(|s| !s.is_empty());
    if let Some(crate_rel) = crate_rel {
        // Nested crate: copy the full workspace source so sibling paths
        // like "../spec/" resolve from the crate's workdir.
        script.push_str(&format!(
            "_bs_workdir=$TMPDIR/workdir/{cr} && \
             cp -r --no-preserve=mode {src}/. $TMPDIR/workdir/ && ",
            cr = crate_rel,
            src = shell_quote(src_store),
        ));
        if has_parent {
            // Move .parent/ content one level above the workspace root
            // so that paths traversing above it still resolve.
            script.push_str(concat!(
                "cp -r --no-preserve=mode $TMPDIR/workdir/.parent/. $TMPDIR/ && ",
                "rm -rf $TMPDIR/workdir/.parent && ",
            ));
        }
    } else {
        script.push_str("_bs_workdir=$TMPDIR/workdir && ");
        script.push_str("cp -r --no-preserve=mode $CARGO_MANIFEST_DIR/. $_bs_workdir && ");
        // For workspace root crates (manifest_dir == src_store), .parent/
        // was included in the copy above; move it one level up.
        // Only applies when the crate is actually local; vendored deps
        // whose manifest_dir is in the vendor store never contain .parent/.
        if has_parent && unit.manifest_dir == src_store {
            script.push_str(concat!(
                "cp -r --no-preserve=mode $_bs_workdir/.parent/. $TMPDIR/ && ",
                "rm -rf $_bs_workdir/.parent && ",
            ));
        }
    }

    // Update CARGO_MANIFEST_DIR to the writable copy so build scripts that
    // read assets via std::env::var("CARGO_MANIFEST_DIR") get a writable path.
    script.push_str("export CARGO_MANIFEST_DIR=$_bs_workdir && ");
    script.push_str("export CARGO_MANIFEST_PATH=$_bs_workdir/Cargo.toml && ");
    // Build scripts that copy files from Nix store paths (vendor deps) can
    // end up with read-only (444/555) permissions on destinations, because
    // fs::copy preserves source permissions via fchmod. When multiple threads
    // copy to the same location, the second thread can't overwrite a 444 file.
    // This LD_PRELOAD shim ensures all files/dirs always retain owner-write.
    script.push_str(concat!(
        r#"cat > $TMPDIR/_wdirs.c << 'WDIRS_EOF'"#,
        "\n",
        "#define _GNU_SOURCE\n",
        "#include <dlfcn.h>\n",
        "#include <sys/stat.h>\n",
        "int chmod(const char *p, mode_t m) {\n",
        "  int (*f)(const char*,mode_t)=dlsym(RTLD_NEXT,\"chmod\");\n",
        "  return f(p,m|0200); }\n",
        "int fchmod(int d, mode_t m) {\n",
        "  int (*f)(int,mode_t)=dlsym(RTLD_NEXT,\"fchmod\");\n",
        "  return f(d,m|0200); }\n",
        "int fchmodat(int d,const char *p, mode_t m, int fl) {\n",
        "  int (*f)(int,const char*,mode_t,int)=dlsym(RTLD_NEXT,\"fchmodat\");\n",
        "  return f(d,p,m|0200,fl); }\n",
        "int mkdir(const char *p, mode_t m) {\n",
        "  int (*f)(const char*,mode_t)=dlsym(RTLD_NEXT,\"mkdir\");\n",
        "  return f(p,m|0200); }\n",
        "int mkdirat(int d, const char *p, mode_t m) {\n",
        "  int (*f)(int,const char*,mode_t)=dlsym(RTLD_NEXT,\"mkdirat\");\n",
        "  return f(d,p,m|0200); }\n",
        "WDIRS_EOF\n",
    ));
    script.push_str("cc -shared -fPIC -o $TMPDIR/_wdirs.so $TMPDIR/_wdirs.c -ldl && ");
    // Setup hook: `$out` already exists (mkdir at the top of this script)
    // and every cargo env export — including the workdir-rewritten
    // CARGO_MANIFEST_DIR — is in place, so scripts see the same
    // environment the build script binary is about to run under.
    script.push_str(&setup_source_fragment(setup_scripts));
    script.push_str(&format!(
        "cd $_bs_workdir && LD_PRELOAD=$TMPDIR/_wdirs.so {}/{} > $out/output",
        bs_placeholder, bs_binary,
    ));
    // Rewrite workdir paths back to the original Nix store path so that
    // cargo:rustc-link-search directives survive to the linking derivation.
    // Use pure bash (no sed) since gnused isn't in the sandbox PATH.
    script.push_str(concat!(
        r#" && _tmp=$out/output.tmp && while IFS= read -r _line; do"#,
        r#" printf '%s\n' "${_line//$_bs_workdir/$_orig_manifest_dir}";"#,
        r#" done < $out/output > $_tmp && mv $_tmp $out/output"#,
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
    use super::derivation_format::{NixDerivation, StoreDir, TargetNix};
    let target = TargetNix::detect()?;
    let store = StoreDir::detect();
    let derivation = NixDerivation::from_ir(json, target, &store)?;
    let json_str = serde_json::to_string(&derivation)?;
    debug!("nix derivation add input ({:?}): {}", target, json_str);
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

    // The project-src root remap that the consumer expresses via
    // `sourceRootPrefix = "crates"`: rewrite the project-src store root to
    // `crates`.
    fn root_remap() -> Vec<(String, String)> {
        vec![(String::new(), "crates".to_string())]
    }

    #[test]
    fn remap_non_sliced_root_maps_store_to_replacement() {
        let args = remap_args(&root_remap(), "/nix/store/h-project-src", None);
        assert_eq!(
            args,
            vec![
                "--remap-path-prefix".to_string(),
                shell_quote("/nix/store/h-project-src=crates"),
            ]
        );
    }

    #[test]
    fn remap_sliced_root_preserves_member_dir() {
        // A per-crate-sliced unit's src_store is the flat `<hash>-<member>`
        // store. The root remap must target `crates/<member>`, not bare
        // `crates`, or the member directory is dropped and every crate's
        // `src/lib.rs` collapses to `crates/src/lib.rs`.
        let args = remap_args(
            &root_remap(),
            "/nix/store/h-skeptiva-ai-common",
            Some("skeptiva-ai-common"),
        );
        assert_eq!(
            args,
            vec![
                "--remap-path-prefix".to_string(),
                shell_quote("/nix/store/h-skeptiva-ai-common=crates/skeptiva-ai-common"),
            ]
        );
    }

    #[test]
    fn remap_sliced_member_under_subdir() {
        // crate_rel carries the full project-src-relative path, including any
        // parent dirs (a non-flat `crates/<member>` workspace layout).
        let args = remap_args(
            &root_remap(),
            "/nix/store/h-msedge-shim",
            Some("crates/skeptiva-ai-msedge-shim"),
        );
        assert_eq!(
            args[1],
            shell_quote("/nix/store/h-msedge-shim=crates/crates/skeptiva-ai-msedge-shim"),
        );
    }

    #[test]
    fn remap_non_root_entry_not_crate_rel_adjusted() {
        // Non-root remaps (e.g. extraSources identity remaps) target a
        // specific subpath; for a sliced crate they reference an external
        // source not under this crate store, so they keep `replacement`
        // verbatim and simply will not match this crate's paths.
        let remaps = vec![("sub/dir".to_string(), "X".to_string())];
        let args = remap_args(&remaps, "/nix/store/h-crate", Some("member"));
        assert_eq!(
            args,
            vec![
                "--remap-path-prefix".to_string(),
                shell_quote("/nix/store/h-crate/sub/dir=X"),
            ]
        );
    }

    #[test]
    fn remap_sorts_shortest_src_relative_first() {
        // rustc resolves overlapping remaps "last matching wins", so the
        // most specific (longest src_relative) must be emitted last.
        let remaps = vec![
            ("aa/bb".to_string(), "deep".to_string()),
            (String::new(), "root".to_string()),
        ];
        let args = remap_args(&remaps, "/nix/store/h-src", None);
        assert_eq!(args[1], shell_quote("/nix/store/h-src=root"));
        assert_eq!(args[3], shell_quote("/nix/store/h-src/aa/bb=deep"));
    }

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

    // -- build_doc_script tests --------------------------------------------------

    fn make_doc_unit(
        name: &str,
        features: &[&str],
        deps: &[(&str, &str)],
        is_local: bool,
    ) -> NixUnit {
        NixUnit {
            key: format!("{}-doc", name),
            drv_name: format!("{}-0.1.0-{}-doc", name, name),
            kind: UnitKind::Doc,
            source_file: format!("/nix/store/fake-src/{}/src/lib.rs", name),
            crate_name: name.replace('-', "_"),
            crate_types: vec!["lib".to_string()],
            edition: "2021".into(),
            features: features.iter().map(|f| f.to_string()).collect(),
            dep_extern: deps
                .iter()
                .map(|(ext, key)| (ext.to_string(), key.to_string()))
                .collect(),
            all_dep_keys: Vec::new(),
            build_script_dep: None,
            build_script_compile_key: None,
            manifest_dir: format!("/nix/store/fake-src/{}", name),
            original_manifest_dir: String::new(),
            cargo_envs: vec![("CARGO_PKG_NAME".into(), name.into())],
            extra_filename: "-abc123".into(),
            needs_linker: false,
            is_local,
            links: None,
            links_dep_keys: Vec::new(),
            is_root: true,
            target_name: name.to_string(),
            for_host: false,
            compile_test: false,
            self_contained_build_script: false,
            sliced_crate_rel: None,
            drv_path: None,
        }
    }

    #[test]
    fn build_doc_script_basic_structure() {
        let unit = make_doc_unit("my-lib", &[], &[], true);
        let units = vec![unit];
        let key_to_idx = HashMap::from([("my-lib-doc".to_string(), 0_usize)]);
        let dep_drv_map = HashMap::new();

        let script = build_doc_script(
            &units[0],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/rustdoc-bin/bin/rustdoc",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            false,
            &[],
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-project-src",
            &[],
        )
        .unwrap();

        // Must invoke rustdoc, not rustc
        assert!(script.contains("/nix/store/rustdoc-bin/bin/rustdoc"));
        assert!(!script.contains("rustc"));
        // Must include --output $out/doc
        assert!(script.contains("--output $out/doc"));
        // Must include --crate-name
        assert!(script.contains("--crate-name my_lib"));
        // Must include --edition
        assert!(script.contains("--edition 2021"));
        // Must include --crate-type
        assert!(script.contains("--crate-type lib"));
        // Must include --sysroot
        assert!(script.contains("--sysroot /nix/store/rust-sysroot"));
        // Must create $out/doc directory
        assert!(script.contains("mkdir"));
        assert!(script.contains("$out/doc"));
        // Must NOT have --emit (rustdoc doesn't use it)
        assert!(!script.contains("--emit"));
        // Must NOT have optimization flags
        assert!(!script.contains("opt-level"));
        // Must capture diagnostics
        assert!(script.contains("diagnostics"));
        // Must NOT have --document-private-items
        assert!(!script.contains("--document-private-items"));
        // With no remaps there is nothing to gate, so no unstable opt-in.
        assert!(!script.contains("-Z unstable-options"));
        assert!(!script.contains("RUSTC_BOOTSTRAP"));
    }

    #[test]
    fn build_doc_script_remap_opts_into_unstable_options() {
        let unit = make_doc_unit("my-lib", &[], &[], true);
        let units = vec![unit];
        let key_to_idx = HashMap::from([("my-lib-doc".to_string(), 0_usize)]);
        let dep_drv_map = HashMap::new();

        let script = build_doc_script(
            &units[0],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/rustdoc-bin/bin/rustdoc",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            false,
            &root_remap(),
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-project-src",
            &[],
        )
        .unwrap();

        // rustdoc gates `--remap-path-prefix` behind `-Z unstable-options`,
        // which the stable-reporting toolchain only accepts with
        // `RUSTC_BOOTSTRAP` set.
        assert!(script.contains("--remap-path-prefix"));
        assert!(script.contains("-Z unstable-options"));
        assert!(script.contains("export RUSTC_BOOTSTRAP=1 &&"));
    }

    #[test]
    fn build_doc_script_private_items() {
        let unit = make_doc_unit("my-lib", &[], &[], true);
        let units = vec![unit];
        let key_to_idx = HashMap::from([("my-lib-doc".to_string(), 0_usize)]);
        let dep_drv_map = HashMap::new();

        let script = build_doc_script(
            &units[0],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/rustdoc-bin/bin/rustdoc",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            true,
            &[],
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-project-src",
            &[],
        )
        .unwrap();

        assert!(script.contains("--document-private-items"));
    }

    #[test]
    fn build_doc_script_private_items_skipped_for_deps() {
        let unit = make_doc_unit("serde", &[], &[], false);
        let units = vec![unit];
        let key_to_idx = HashMap::from([("serde-doc".to_string(), 0_usize)]);
        let dep_drv_map = HashMap::new();

        let script = build_doc_script(
            &units[0],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/rustdoc-bin/bin/rustdoc",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            true,
            &[],
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-project-src",
            &[],
        )
        .unwrap();

        // --document-private-items should only apply to local crates
        assert!(!script.contains("--document-private-items"));
    }

    #[test]
    fn build_doc_script_with_features() {
        let unit = make_doc_unit("my-lib", &["serde", "async"], &[], true);
        let units = vec![unit];
        let key_to_idx = HashMap::from([("my-lib-doc".to_string(), 0_usize)]);
        let dep_drv_map = HashMap::new();

        let script = build_doc_script(
            &units[0],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/rustdoc-bin/bin/rustdoc",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            false,
            &[],
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-project-src",
            &[],
        )
        .unwrap();

        assert!(script.contains(r#"--cfg 'feature="serde"'"#));
        assert!(script.contains(r#"--cfg 'feature="async"'"#));
    }

    #[test]
    fn build_doc_script_cap_lints_for_deps() {
        let unit = make_doc_unit("serde", &[], &[], false);
        let units = vec![unit];
        let key_to_idx = HashMap::from([("serde-doc".to_string(), 0_usize)]);
        let dep_drv_map = HashMap::new();

        let script = build_doc_script(
            &units[0],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/rustdoc-bin/bin/rustdoc",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            false,
            &[],
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-project-src",
            &[],
        )
        .unwrap();

        assert!(script.contains("--cap-lints allow"));
    }

    // -- unit setup script tests ---------------------------------------------

    const SETUP_A: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-setup-a.sh";
    const SETUP_B: &str = "/nix/store/cccccccccccccccccccccccccccccccc-setup-b.sh";
    const SRC_STORE: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-project-src";

    fn make_check_unit(name: &str) -> NixUnit {
        let mut unit = make_doc_unit(name, &[], &[], true);
        unit.kind = UnitKind::Check;
        unit.key = format!("{}-check", name);
        unit
    }

    fn check_script(setup_scripts: &[String]) -> String {
        let units = vec![make_check_unit("my-lib")];
        let key_to_idx = HashMap::from([("my-lib-check".to_string(), 0_usize)]);
        build_compile_script(
            &units[0],
            &units,
            &key_to_idx,
            &HashMap::new(),
            "/nix/store/rustc-bin/bin/rustc",
            "",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            "/nix/store/cc/bin",
            &ProfileConfig::dev(),
            &TargetConfig::native(),
            &[],
            &[],
            &[],
            SRC_STORE,
            setup_scripts,
        )
        .unwrap()
    }

    #[test]
    fn check_script_sources_setup_between_envs_and_driver() {
        // The sourcing fragment sits after the CARGO_MANIFEST_DIR export
        // and before the driver invocation, with `mkdir -p $out` and the
        // SCHNEE_AUX_DIR export preceding it — so a script's exports are
        // readable, $out exists, and its exports reach the driver.
        let script = check_script(&[SETUP_A.to_string()]);
        let manifest = script.find("export CARGO_MANIFEST_DIR=").unwrap();
        let mkdir = script
            .find("/nix/store/coreutils/bin/mkdir -p $out && ")
            .unwrap();
        let aux = script
            .find("export SCHNEE_AUX_DIR=$out/schnee-aux && ")
            .unwrap();
        let source = script.find(&format!(". {} && ", SETUP_A)).unwrap();
        let driver = script.find("/nix/store/rustc-bin/bin/rustc").unwrap();
        assert!(manifest < mkdir, "mkdir must follow the cargo env exports");
        assert!(mkdir < aux, "$out must exist before SCHNEE_AUX_DIR is set");
        assert!(
            aux < source,
            "SCHNEE_AUX_DIR must be exported before sourcing"
        );
        assert!(
            source < driver,
            "scripts must source before the driver runs"
        );
    }

    #[test]
    fn check_script_sources_two_scripts_in_list_order() {
        let script = check_script(&[SETUP_A.to_string(), SETUP_B.to_string()]);
        let a = script.find(&format!(". {} && ", SETUP_A)).unwrap();
        let b = script.find(&format!(". {} && ", SETUP_B)).unwrap();
        assert!(a < b, "matched scripts must source in rule order");
    }

    #[test]
    fn check_script_without_setup_is_byte_identical_to_pre_feature_shape() {
        // No matched scripts: no hook artefacts at all, and the mkdir
        // stays directly adjacent to the driver invocation — the exact
        // pre-feature byte layout.
        let script = check_script(&[]);
        assert!(!script.contains("SCHNEE_AUX_DIR"));
        assert!(
            script.contains(
                "/nix/store/coreutils/bin/mkdir -p $out && /nix/store/rustc-bin/bin/rustc"
            )
        );
        // And the with-scripts variant differs only by the inserted
        // fragment: removing it restores the byte-identical script.
        let with = check_script(&[SETUP_A.to_string()]);
        let fragment = format!("export SCHNEE_AUX_DIR=$out/schnee-aux && . {} && ", SETUP_A);
        assert_eq!(with.replacen(&fragment, "", 1), script);
    }

    #[test]
    fn doc_script_sources_setup_between_mkdir_and_rustdoc() {
        let unit = make_doc_unit("my-lib", &[], &[], true);
        let units = vec![unit];
        let key_to_idx = HashMap::from([("my-lib-doc".to_string(), 0_usize)]);
        let script = build_doc_script(
            &units[0],
            &units,
            &key_to_idx,
            &HashMap::new(),
            "/nix/store/rustdoc-bin/bin/rustdoc",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            false,
            &[],
            SRC_STORE,
            &[SETUP_A.to_string()],
        )
        .unwrap();
        let mkdir = script
            .find("/nix/store/coreutils/bin/mkdir -p $out/doc && ")
            .unwrap();
        let aux = script
            .find("export SCHNEE_AUX_DIR=$out/schnee-aux && ")
            .unwrap();
        let source = script.find(&format!(". {} && ", SETUP_A)).unwrap();
        let driver = script.find("/nix/store/rustdoc-bin/bin/rustdoc").unwrap();
        assert!(mkdir < aux && aux < source && source < driver);
    }

    #[test]
    fn run_script_sources_setup_before_build_script_binary() {
        let mut bs_compile = make_doc_unit("my-lib", &[], &[], true);
        bs_compile.kind = UnitKind::BuildScriptCompile;
        bs_compile.key = "my-lib-bsc".into();
        bs_compile.crate_name = "build_script_build".into();
        bs_compile.crate_types = vec!["bin".into()];
        let mut run = make_doc_unit("my-lib", &[], &[], true);
        run.kind = UnitKind::BuildScriptRun;
        run.key = "my-lib-bsr".into();
        run.build_script_compile_key = Some("my-lib-bsc".into());
        let units = vec![bs_compile, run];
        let key_to_idx = HashMap::from([
            ("my-lib-bsc".to_string(), 0_usize),
            ("my-lib-bsr".to_string(), 1_usize),
        ]);
        let dep_drv_map = HashMap::from([(
            "my-lib-bsc".to_string(),
            "/nix/store/dddddddddddddddddddddddddddddddd-bs.drv".to_string(),
        )]);
        let script = build_run_script(
            &units[1],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/coreutils/bin/mkdir",
            "/nix/store/coreutils",
            "/nix/store/rustc-bin/bin/rustc",
            "/nix/store/cc/bin",
            &None,
            "",
            &ProfileConfig::dev(),
            &TargetConfig::native(),
            &[],
            &[],
            &[],
            &[],
            SRC_STORE,
            &[SETUP_A.to_string()],
        )
        .unwrap();
        let mkdir = script
            .find("/nix/store/coreutils/bin/mkdir -p $out $out/out_dir && ")
            .unwrap();
        let manifest = script.rfind("export CARGO_MANIFEST_DIR=").unwrap();
        let aux = script
            .find("export SCHNEE_AUX_DIR=$out/schnee-aux && ")
            .unwrap();
        let source = script.find(&format!(". {} && ", SETUP_A)).unwrap();
        let exec = script.find("cd $_bs_workdir && LD_PRELOAD=").unwrap();
        assert!(mkdir < aux, "$out must exist before SCHNEE_AUX_DIR is set");
        assert!(
            manifest < aux,
            "the hook must follow the workdir-rewritten CARGO_MANIFEST_DIR export"
        );
        assert!(aux < source && source < exec);
    }

    #[test]
    fn run_script_without_setup_has_no_hook_artefacts() {
        let mut bs_compile = make_doc_unit("my-lib", &[], &[], true);
        bs_compile.kind = UnitKind::BuildScriptCompile;
        bs_compile.key = "my-lib-bsc".into();
        bs_compile.crate_types = vec!["bin".into()];
        let mut run = make_doc_unit("my-lib", &[], &[], true);
        run.kind = UnitKind::BuildScriptRun;
        run.key = "my-lib-bsr".into();
        run.build_script_compile_key = Some("my-lib-bsc".into());
        let units = vec![bs_compile, run];
        let key_to_idx = HashMap::from([
            ("my-lib-bsc".to_string(), 0_usize),
            ("my-lib-bsr".to_string(), 1_usize),
        ]);
        let dep_drv_map = HashMap::from([(
            "my-lib-bsc".to_string(),
            "/nix/store/dddddddddddddddddddddddddddddddd-bs.drv".to_string(),
        )]);
        let script = build_run_script(
            &units[1],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/coreutils/bin/mkdir",
            "/nix/store/coreutils",
            "/nix/store/rustc-bin/bin/rustc",
            "/nix/store/cc/bin",
            &None,
            "",
            &ProfileConfig::dev(),
            &TargetConfig::native(),
            &[],
            &[],
            &[],
            &[],
            SRC_STORE,
            &[],
        )
        .unwrap();
        assert!(!script.contains("SCHNEE_AUX_DIR"));
        assert!(script.contains("_wdirs.c -ldl && cd $_bs_workdir"));
    }

    #[test]
    fn build_doc_script_no_cap_lints_for_local() {
        let unit = make_doc_unit("my-lib", &[], &[], true);
        let units = vec![unit];
        let key_to_idx = HashMap::from([("my-lib-doc".to_string(), 0_usize)]);
        let dep_drv_map = HashMap::new();

        let script = build_doc_script(
            &units[0],
            &units,
            &key_to_idx,
            &dep_drv_map,
            "/nix/store/rustdoc-bin/bin/rustdoc",
            "/nix/store/rust-sysroot",
            "/nix/store/coreutils/bin",
            false,
            &[],
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-project-src",
            &[],
        )
        .unwrap();

        assert!(!script.contains("--cap-lints"));
    }
}
