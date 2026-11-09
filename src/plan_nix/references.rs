//! Runtime references of unit outputs.
//!
//! Nix records as a reference every input whose hash appears anywhere in an
//! output, so a source path in a panic message, a header path in DWARF or
//! a library path in a PDB keeps that input alive at run time. Two
//! mechanisms keep build-time inputs out:
//!
//! - Each compiler sees every input it may name under a hash-free path,
//!   `/schnee/<name>`, through `--remap-path-prefix` for rustc and rustdoc
//!   and `-ffile-prefix-map` for C and C++ compiled by build scripts.
//! - A unit that produces a runtime artefact declares what its output may
//!   refer to in `allowedReferences`, and `schnee-refs` names the file and
//!   the bytes behind any other reference before Nix rejects the output.

use super::derivation::downstream_placeholder;
use super::util::shell_quote;
use super::{NixUnit, TargetConfig, UnitKind};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The directory every remapped input appears under.
pub(crate) const VIRTUAL_ROOT: &str = "/schnee";

/// The `/schnee/<name>` path for the store path `path`, which must be a
/// store root such as `/nix/store/<hash>-<name>`.
pub(crate) fn virtual_path(path: &str) -> Option<String> {
    let base = Path::new(path).file_name()?.to_str()?;
    let name = base
        .get(33..)
        .filter(|_| base.as_bytes().get(32) == Some(&b'-'))?;
    Some(format!("{VIRTUAL_ROOT}/{name}"))
}

/// A path a compiler may see, with the hash-free path it reports instead.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Remap {
    from: String,
    to: String,
}

impl Remap {
    /// The remap of a store root.
    pub(super) fn store(path: &str) -> Option<Self> {
        Some(Self {
            from: path.to_string(),
            to: virtual_path(path)?,
        })
    }

    /// The remap of the output of the derivation at `drv_path`, which a
    /// unit names through its placeholder.
    pub(super) fn output_of(drv_path: &str) -> Result<Self> {
        let name = Path::new(drv_path)
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.get(33..))
            .and_then(|n| n.strip_suffix(".drv"))
            .with_context(|| format!("malformed derivation path {drv_path}"))?;
        Ok(Self {
            from: downstream_placeholder(drv_path, "out")?,
            to: format!("{VIRTUAL_ROOT}/{name}"),
        })
    }

    /// The remap of the unit's own output, under the unit's name.
    pub(super) fn own_output(unit: &NixUnit) -> Self {
        Self {
            from: "$out".into(),
            to: format!("{VIRTUAL_ROOT}/{}", unit.drv_name),
        }
    }
}

/// Sorted, deduplicated `remaps`, so a derivation does not depend on the
/// order its inputs were found in.
pub(super) fn normalised(mut remaps: Vec<Remap>) -> Vec<Remap> {
    remaps.sort();
    remaps.dedup();
    remaps
}

/// `--remap-path-prefix` arguments for rustc or rustdoc.
pub(super) fn rustc_args(remaps: &[Remap]) -> Vec<String> {
    remaps
        .iter()
        .flat_map(|r| {
            [
                "--remap-path-prefix".to_string(),
                format!("\"{}={}\"", r.from, r.to),
            ]
        })
        .collect()
}

/// Shell fragment exporting `-ffile-prefix-map` for every remap in
/// `CFLAGS` and `CXXFLAGS`, ahead of any value already set.
///
/// The cc crate prefers `CFLAGS_<target>` over `CFLAGS`, so an MSVC target
/// gets its own variables, which also keep the compiler's command line out
/// of the CodeView data because it names the compiler's resource directory.
/// Their form follows the compiler `CC_<target>` or `CXX_<target>` names,
/// which is known once the passthrough variables are set. clang-cl takes
/// each flag through `/clang:`, and the plain clang driver, which ring
/// requires for Windows AArch64, reads such a flag as an input file. Any
/// other compiler gets the plain form, which clang-cl would only warn about.
pub(super) fn c_flag_exports(remaps: &[Remap], target: &TargetConfig) -> String {
    let maps: Vec<String> = remaps
        .iter()
        .map(|r| format!("-ffile-prefix-map={}={}", r.from, r.to))
        .collect();
    let export =
        |var: &str, value: &str| format!("export {var}=\"{value}${{{var}:+ ${var}}}\" && ");
    let mut s = format!("__maps=\"{}\" && ", maps.join(" "));
    s += &export("CFLAGS", "$__maps");
    s += &export("CXXFLAGS", "$__maps");
    if target.is_msvc() {
        let triple = target.target_triple.replace('-', "_");
        for (compiler, flags) in [("CC", "CFLAGS"), ("CXX", "CXXFLAGS")] {
            s += &format!(
                r#"case "${{{compiler}_{triple}:-$CC_{triple}}}" in */clang-cl*|clang-cl*) __f=""; for __m in $__maps -gno-codeview-command-line; do __f="$__f${{__f:+ }}/clang:$__m"; done ;; *) __f="$__maps -gno-codeview-command-line" ;; esac && "#
            );
            s += &export(&format!("{flags}_{triple}"), "$__f");
        }
    }
    s
}

/// Shell fragment, run where a build script's `cargo:rustc-link-search`
/// value is in `$d`, adding the store root it names to `__linked`.
pub(super) fn link_search_root(store_dir: &str) -> String {
    format!(
        r#"d="${{d#*=}}"; case "$d" in {store_dir}/*) r="${{d#{store_dir}/}}"; __linked="$__linked {store_dir}/${{r%%/*}}" ;; esac; "#
    )
}

/// Shell fragment, run where a build script's `cargo:rustc-env` value is in
/// `$v`, adding a remap for the store root the value names. A path that
/// reaches rustc this way, such as a code generator's output directory
/// that `include!` reads, is known only once the build script has run.
pub(super) fn rustc_env_remap(store_dir: &str) -> String {
    format!(
        r#"case "$v" in {store_dir}/*) r="${{v#{store_dir}/}}"; r="${{r%%/*}}"; EXTRA_ARGS="$EXTRA_ARGS --remap-path-prefix={store_dir}/$r={VIRTUAL_ROOT}/${{r:33}}" ;; esac; "#
    )
}

/// What every unit's references depend on beyond the unit itself.
pub(super) struct Policy<'a> {
    /// The derivation that builds `schnee-refs`.
    pub(super) tool_drv: &'a str,
    /// What a linked target binary may refer to, see [`linked_runtime`].
    pub(super) runtime: &'a [String],
    /// The paths a `runtime-references` name may match.
    pub(super) known: &'a [String],
    /// Keys of the units whose code reaches a root, see [`runtime_units`].
    pub(super) runtime_units: &'a HashSet<String>,
}

/// Keys of the units whose code ends up in a root unit's output: the roots
/// and the libraries they link, directly or through other libraries. A proc
/// macro, a build script and what only they depend on run during the build
/// alone. `for_host` cannot tell them apart, because a build without
/// `--target` compiles every unit for the host.
pub(super) fn runtime_units(
    units: &[NixUnit],
    key_to_idx: &HashMap<String, usize>,
) -> HashSet<String> {
    let is_proc_macro = |u: &NixUnit| u.crate_types.iter().any(|t| t == "proc-macro");
    let mut seen: HashSet<String> = HashSet::new();
    let mut pending: Vec<&NixUnit> = units
        .iter()
        .filter(|u| u.is_root && !is_proc_macro(u))
        .collect();
    while let Some(unit) = pending.pop() {
        if !seen.insert(unit.key.clone()) {
            continue;
        }
        pending.extend(
            unit.dep_extern
                .iter()
                .filter_map(|(_, key)| key_to_idx.get(key).map(|&i| &units[i]))
                .filter(|dep| !is_proc_macro(dep)),
        );
    }
    seen
}

/// What a unit's output may refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Contract {
    /// The output is a build-time intermediate, such as a build script's
    /// output or a proc macro, and may refer to anything.
    Unchecked,
    /// The output may refer to these store paths alone, which Nix checks
    /// through `allowedReferences`.
    Allowed(Vec<String>),
    /// The output may refer to these store paths and the store roots of the
    /// `cargo:rustc-link-search` directories it links against, because the
    /// linker records a directory it found a shared library in. Those roots
    /// are known only once the build scripts have run, and listing every
    /// library pkg-config could hand out would re-key the unit on each of
    /// them, so `schnee-refs` alone checks this contract.
    Linked(Vec<String>),
}

/// The runtime closure Nix's toolchain links every Linux binary against,
/// that is the C library and the GCC runtime, read from the cc wrapper at
/// `cc_store`. Empty for a compiler that is not a Nix cc wrapper.
pub(super) fn linked_runtime(cc_store: &str, store_dir: &str) -> Vec<String> {
    let support = Path::new(cc_store).join("nix-support");
    let read = |name: &str| std::fs::read_to_string(support.join(name)).unwrap_or_default();
    let compiler = read("orig-cc");
    let compiler = compiler.trim();
    let mut paths: Vec<String> = Vec::new();
    let libc = read("orig-libc");
    paths.extend(libc.split_whitespace().map(str::to_string));
    for flag in read("cc-ldflags").split_whitespace() {
        let Some(dir) = flag.strip_prefix("-L") else {
            continue;
        };
        if let Some(root) = super::store_paths::store_path_of(dir, store_dir)
            && root != compiler
        {
            paths.push(root.to_string());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// The store paths `[package.metadata.schnee] runtime-references` in the
/// manifest of `unit` adds to its runtime references. An entry is a store
/// path or a name. A name matches each path in `known` whose name is the
/// entry, or the entry followed by `-` and a version.
pub(super) fn declared_runtime_references(unit: &NixUnit, known: &[String]) -> Result<Vec<String>> {
    let manifest_path = Path::new(&unit.manifest_dir).join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap_or_default();
    let entries = super::util::package_runtime_references(&manifest);
    let mut found = Vec::new();
    for entry in entries {
        if entry.starts_with('/') {
            found.push(entry);
            continue;
        }
        let matches: Vec<&String> = known
            .iter()
            .filter(|p| {
                let name = virtual_path(p).unwrap_or_default();
                let name = name
                    .trim_start_matches(VIRTUAL_ROOT)
                    .trim_start_matches('/');
                name == entry
                    || name
                        .strip_prefix(&format!("{entry}-"))
                        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
            })
            .collect();
        anyhow::ensure!(
            !matches.is_empty(),
            "runtime-references entry {entry:?} in {} names no store path this build links against",
            manifest_path.display()
        );
        found.extend(matches.into_iter().cloned());
    }
    Ok(found)
}

/// The contract of `unit`. A runtime unit that compiles a library may refer
/// to nothing, because an rlib is linked rather than loaded. A runtime unit
/// that links may refer to the policy's runtime paths, the libraries it
/// links and what its manifest declares, except on Windows, where nothing
/// loads a store path at run time. Other units, tests among them, only run
/// during the build.
pub(super) fn contract(
    unit: &NixUnit,
    target: &TargetConfig,
    policy: &Policy<'_>,
) -> Result<Contract> {
    if unit.kind != UnitKind::Compile || !policy.runtime_units.contains(&unit.key) {
        return Ok(Contract::Unchecked);
    }
    if !unit.needs_linker || target.is_windows() {
        return Ok(Contract::Allowed(Vec::new()));
    }
    let mut allowed = policy.runtime.to_vec();
    allowed.extend(declared_runtime_references(unit, policy.known)?);
    allowed.sort();
    allowed.dedup();
    Ok(Contract::Linked(allowed))
}

/// Shell fragment that runs `schnee-refs` over `$out` once the compiler
/// has succeeded. It rewrites the diagnostics to hash-free paths, scrubs
/// the CodeView records of an MSVC target, and fails the unit with a
/// report on any other reference outside `contract`. `__linked` holds the
/// link-search roots the compile script collected.
pub(super) fn check_fragment(
    tool_drv: &str,
    target: &TargetConfig,
    contract: &Contract,
) -> Result<String> {
    let allowed = match contract {
        Contract::Unchecked => return Ok(String::new()),
        Contract::Allowed(_) => r#""$allowedReferences""#,
        Contract::Linked(_) => r#""$schneeRuntimeReferences $__linked""#,
    };
    let tool = downstream_placeholder(tool_drv, "out")?;
    let scrub = if target.is_msvc() {
        " --scrub .pdb --scrub .rlib --scrub .lib"
    } else {
        ""
    };
    Ok(format!(
        r#"if [ $__rs = 0 ]; then {tool}/bin/schnee-refs $out --allowed {allowed} --rewrite diagnostics{scrub} || __rs=$?; fi; "#
    ))
}

/// The derivation that compiles `schnee-refs` with the host toolchain.
#[allow(clippy::too_many_arguments)]
pub(super) fn tool_drv_json(
    rustc_path: &str,
    sysroot: &str,
    rustc_closure: &[String],
    host_cc_bin_dir: &str,
    host_cc_closure: &[String],
    bash_path: &str,
    bash_store: &str,
    coreutils_store: &str,
    system: &str,
) -> serde_json::Value {
    let source = format!(
        "{}\nfn main() {{\n    let args: Vec<String> = std::env::args().skip(1).collect();\n    std::process::exit(run(&args));\n}}\n",
        include_str!("refs_tool.rs")
    );
    let script = format!(
        "export PATH={cc}:{coreutils}/bin && mkdir -p $out/bin && printf '%s' \"$source\" > $TMPDIR/schnee_refs.rs && {rustc} --sysroot {sysroot} --edition 2021 --crate-name schnee_refs --crate-type bin -C opt-level=2 -o $out/bin/schnee-refs $TMPDIR/schnee_refs.rs",
        cc = shell_quote(host_cc_bin_dir),
        coreutils = coreutils_store,
        rustc = shell_quote(rustc_path),
    );
    // Like a unit, the derivation also mounts every store path its script
    // runs, such as nixpkgs' rustc wrapper, which lives outside the sysroot
    // `rustc_closure` was queried for.
    let mut input_srcs: std::collections::BTreeSet<String> = rustc_closure
        .iter()
        .chain(host_cc_closure)
        .cloned()
        .chain([bash_store.to_string(), coreutils_store.to_string()])
        .collect();
    let mut named = HashSet::new();
    super::util::collect_store_paths(&script, &mut named);
    input_srcs.extend(named);
    let input_srcs: Vec<String> = input_srcs.into_iter().collect();
    serde_json::json!({
        "name": "schnee-refs",
        "system": system,
        "builder": bash_path,
        "args": ["-c", script],
        "env": {
            "out": super::derivation::self_placeholder("out"),
            "source": source,
            "preferLocalBuild": "1",
        },
        "inputDrvs": {},
        "inputSrcs": input_srcs,
        "outputs": { "out": { "hashAlgo": "sha256", "method": "nar" } }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_path_drops_the_store_directory_and_the_hash() {
        assert_eq!(
            virtual_path("/nix/store/0hqrgnc1vab3i7nz3ysx9dcdill8kwk9-rust-default-1.98.0")
                .as_deref(),
            Some("/schnee/rust-default-1.98.0")
        );
        assert_eq!(virtual_path("/nix/store/short-name"), None);
    }

    #[test]
    fn rustc_args_quote_each_mapping() {
        let remap = Remap::store("/nix/store/0hqrgnc1vab3i7nz3ysx9dcdill8kwk9-src").unwrap();
        assert_eq!(
            rustc_args(&[remap]),
            vec![
                "--remap-path-prefix".to_string(),
                "\"/nix/store/0hqrgnc1vab3i7nz3ysx9dcdill8kwk9-src=/schnee/src\"".to_string()
            ]
        );
    }

    /// The value of `var` after the shell runs `fragment` with `env` set.
    /// The fragment is POSIX, and `sh` is the one shell both a developer's
    /// machine and the package build's sandbox, whose `PATH` holds no
    /// bash, provide.
    fn exported(fragment: &str, env: &[(&str, &str)], var: &str) -> String {
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{fragment}printf '%s' \"${var}\""))
            .env_clear()
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn glibc() -> Remap {
        Remap::store("/nix/store/0hqrgnc1vab3i7nz3ysx9dcdill8kwk9-glibc-2.44").unwrap()
    }

    const MAP: &str = "-ffile-prefix-map=/nix/store/0hqrgnc1vab3i7nz3ysx9dcdill8kwk9-glibc-2.44=/schnee/glibc-2.44";

    #[test]
    fn c_flags_keep_a_value_already_set() {
        let target = TargetConfig::with_target("x86_64-unknown-linux-gnu");
        let exports = c_flag_exports(&[glibc()], &target);
        assert_eq!(
            exported(&exports, &[("CFLAGS", "-O2")], "CFLAGS"),
            format!("{MAP} -O2")
        );
        assert_eq!(exported(&exports, &[], "CXXFLAGS"), MAP);
    }

    /// ring rejects clang-cl for Windows AArch64, so that target's build
    /// scripts compile with the plain clang driver, which reads a `/clang:`
    /// flag as an input file.
    #[test]
    fn c_flags_for_the_plain_clang_driver_take_the_gnu_form() {
        let target = TargetConfig::with_target("aarch64-pc-windows-msvc");
        let exports = c_flag_exports(&[glibc()], &target);
        let env = [(
            "CC_aarch64_pc_windows_msvc",
            "/nix/store/x-clang-21/bin/clang",
        )];
        assert_eq!(
            exported(&exports, &env, "CFLAGS_aarch64_pc_windows_msvc"),
            format!("{MAP} -gno-codeview-command-line")
        );
        assert_eq!(
            exported(&exports, &env, "CXXFLAGS_aarch64_pc_windows_msvc"),
            format!("{MAP} -gno-codeview-command-line")
        );
    }

    #[test]
    fn c_flags_for_clang_cl_take_the_clang_form() {
        let target = TargetConfig::with_target("x86_64-pc-windows-msvc");
        let exports = c_flag_exports(&[glibc()], &target);
        let env = [(
            "CC_x86_64_pc_windows_msvc",
            "/nix/store/x-clang-21/bin/clang-cl",
        )];
        assert_eq!(
            exported(&exports, &env, "CFLAGS_x86_64_pc_windows_msvc"),
            format!("/clang:{MAP} /clang:-gno-codeview-command-line")
        );
        assert_eq!(exported(&exports, &env, "CFLAGS"), MAP);
    }

    /// nixpkgs' rustc is a wrapper outside the sysroot whose closure the
    /// planner queries, so the tool derivation must mount it from the path
    /// its script runs.
    #[test]
    fn tool_drv_mounts_a_wrapped_rustc() {
        let wrapper = "/nix/store/6l9kbzxwq9ij2lcmwl2qb1jdpqisjr8l-rustc-wrapper-1.99.0";
        let sysroot = "/nix/store/1qg7z7i461qsv0lczc40nyi8ps6snlca-rustc-1.99.0";
        let json = tool_drv_json(
            &format!("{wrapper}/bin/rustc"),
            sysroot,
            &[sysroot.to_string()],
            "/nix/store/r5ri78axx1qggkjq3vawywlzglg70b5w-gcc-wrapper-16.2.0/bin",
            &[],
            "/nix/store/nnm9gdr70ihc6zc9298mzz8jpcgaciql-bash-5.3p20/bin/bash",
            "/nix/store/nnm9gdr70ihc6zc9298mzz8jpcgaciql-bash-5.3p20",
            "/nix/store/cb987zkxvsv0490753ilrvn5yk7ylgxi-coreutils-9.12",
            "x86_64-linux",
        );
        let srcs: Vec<&str> = json["inputSrcs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(srcs.contains(&wrapper), "{srcs:?}");
    }

    #[test]
    fn linked_runtime_reads_libc_and_the_gcc_runtime() {
        let dir = std::env::temp_dir().join(format!("schnee-cc-wrapper-{}", std::process::id()));
        let support = dir.join("nix-support");
        std::fs::create_dir_all(&support).unwrap();
        let gcc = "/nix/store/xpaz3fi6ibqxb9sla4gjlp0c82v566y8-gcc-16.2.0";
        let lib = "/nix/store/7a2cjhcgzyym5zaz6m4fksw47qvm150w-gcc-16.2.0-lib";
        let libc = "/nix/store/pqyiidz5mi73wv5f91d48s9plcqyj5bz-glibc-2.44-57";
        std::fs::write(support.join("orig-cc"), format!("{gcc}\n")).unwrap();
        std::fs::write(support.join("orig-libc"), format!("{libc}\n")).unwrap();
        std::fs::write(
            support.join("cc-ldflags"),
            format!("-L{gcc}/lib/gcc/x86_64-unknown-linux-gnu/16.2.0 -L{lib}/lib -L{lib}/lib "),
        )
        .unwrap();
        let runtime = linked_runtime(&dir.to_string_lossy(), "/nix/store");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(runtime, vec![lib.to_string(), libc.to_string()]);
    }
}
