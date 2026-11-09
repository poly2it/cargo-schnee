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
/// The cc crate prefers `CFLAGS_<target>` over `CFLAGS`, so a target whose
/// compiler is clang-cl gets its own variables, with each flag passed
/// through `/clang:` because clang-cl accepts no `-f` flag of its own.
/// clang-cl also leaves its `-cc1` command line out of the CodeView data,
/// because the command line names the compiler's resource directory.
pub(super) fn c_flag_exports(remaps: &[Remap], target: &TargetConfig) -> String {
    let flags = |prefix: &str| {
        remaps
            .iter()
            .map(|r| format!("{prefix}-ffile-prefix-map={}={}", r.from, r.to))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let export =
        |var: &str, value: &str| format!("export {var}=\"{value}${{{var}:+ ${var}}}\" && ");
    let gnu = flags("");
    let mut s = export("CFLAGS", &gnu) + &export("CXXFLAGS", &gnu);
    if target.is_msvc() {
        let triple = target.target_triple.replace('-', "_");
        let cl = format!("{} /clang:-gno-codeview-command-line", flags("/clang:"));
        s += &export(&format!("CFLAGS_{triple}"), &cl);
        s += &export(&format!("CXXFLAGS_{triple}"), &cl);
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
/// the CodeView records of an MSVC target, and fails the unit with a report on any other
/// reference outside `contract`. `__linked` holds the link-search roots
/// the compile script collected.
pub(super) fn check_fragment(
    tool_drv: &str,
    unit: &NixUnit,
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
    let mut input_srcs: Vec<String> = rustc_closure
        .iter()
        .chain(host_cc_closure)
        .cloned()
        .chain([bash_store.to_string(), coreutils_store.to_string()])
        .collect();
    input_srcs.sort();
    input_srcs.dedup();
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

    #[test]
    fn c_flags_keep_a_value_already_set() {
        let remap = Remap::store("/nix/store/0hqrgnc1vab3i7nz3ysx9dcdill8kwk9-glibc-2.44").unwrap();
        let exports = c_flag_exports(
            &[remap],
            &TargetConfig::with_target("x86_64-unknown-linux-gnu"),
        );
        assert_eq!(
            exports,
            "export CFLAGS=\"-ffile-prefix-map=/nix/store/0hqrgnc1vab3i7nz3ysx9dcdill8kwk9-glibc-2.44=/schnee/glibc-2.44${CFLAGS:+ $CFLAGS}\" && \
             export CXXFLAGS=\"-ffile-prefix-map=/nix/store/0hqrgnc1vab3i7nz3ysx9dcdill8kwk9-glibc-2.44=/schnee/glibc-2.44${CXXFLAGS:+ $CXXFLAGS}\" && "
        );
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
