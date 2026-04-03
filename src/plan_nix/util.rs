use crate::nix_encoding::NIX_BASE32;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub(crate) fn collect_store_paths(s: &str, paths: &mut HashSet<String>) {
    let mut search_from = 0;
    while let Some(idx) = s[search_from..].find("/nix/store/") {
        let start = search_from + idx;
        let after_prefix = &s[start + "/nix/store/".len()..];
        if after_prefix.len() >= 32 && after_prefix.as_bytes().get(32) == Some(&b'-') {
            // Validate that the 32-char hash uses only valid Nix base32 chars.
            // Placeholder paths (e.g. clang wrapper's eeee...eeee-gcc) use
            // characters outside the alphabet and must be skipped.
            let hash_part = &after_prefix[..32];
            if hash_part.bytes().all(|b| NIX_BASE32.contains(&b)) {
                let rest = &after_prefix[33..];
                let name_end = rest.find(['/', ' ', '"', '\'', ')']).unwrap_or(rest.len());
                let root = &s[start..start + "/nix/store/".len() + 32 + 1 + name_end];
                paths.insert(root.to_string());
            }
        }
        search_from = start + 1;
    }
}

pub(super) fn which_rustc() -> Result<PathBuf> {
    which_command("rustc")
}

pub(super) fn which_command_no_deref(name: &str) -> Result<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = PathBuf::from(dir).join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!(
        "{} not found in PATH. \
         Ensure it is installed, or use 'nix develop' to enter the dev shell.",
        name
    );
}

pub(super) fn which_command(name: &str) -> Result<PathBuf> {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = PathBuf::from(dir).join(name);
            if candidate.exists() {
                return candidate
                    .canonicalize()
                    .with_context(|| format!("Failed to canonicalize {} path", name));
            }
        }
    }
    anyhow::bail!(
        "{} not found in PATH. \
         Ensure it is installed, or use 'nix develop' to enter the dev shell.",
        name
    );
}

/// Find a cross-compilation linker for the given target triple.
/// Checks CARGO_TARGET_{TRIPLE}_LINKER env var, then searches PATH.
/// For MSVC targets, searches for lld-link; for GNU targets, searches for {triple}-gcc/cc.
pub(super) fn find_cross_linker(target_triple: &str) -> Result<PathBuf> {
    let env_key = format!(
        "CARGO_TARGET_{}_LINKER",
        target_triple.to_uppercase().replace('-', "_")
    );
    if let Ok(linker) = std::env::var(&env_key) {
        let path = PathBuf::from(&linker);
        if path.exists() {
            return Ok(path);
        }
        // The env var might be a bare command name — try PATH lookup
        if let Ok(resolved) = which_command_no_deref(&linker) {
            return Ok(resolved);
        }
        anyhow::bail!(
            "Cross-linker specified by {} = '{}' not found",
            env_key,
            linker
        );
    }
    let is_msvc = target_triple.contains("msvc");
    let candidates: &[&str] = if is_msvc {
        &["lld-link", "rust-lld"]
    } else {
        &["gcc", "cc"]
    };
    for suffix in candidates {
        let name = if is_msvc {
            suffix.to_string()
        } else {
            format!("{}-{}", target_triple, suffix)
        };
        if let Ok(path) = which_command_no_deref(&name) {
            return Ok(path);
        }
    }
    if is_msvc {
        anyhow::bail!(
            "Cross-linker not found for target {}. \
             Set {} or add lld-link to PATH.",
            target_triple,
            env_key,
        );
    } else {
        anyhow::bail!(
            "Cross-linker not found for target {}. \
             Set {} or add {}-gcc to PATH.",
            target_triple,
            env_key,
            target_triple
        );
    }
}

/// Find a sysroot rlib by name, resolving symlinks to get the real path.
pub(super) fn find_sysroot_rlib(sysroot_lib: &Path, crate_name: &str) -> Result<String> {
    let prefix = format!("lib{}-", crate_name);
    if sysroot_lib.exists() {
        for entry in std::fs::read_dir(sysroot_lib)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(".rlib") {
                // Resolve symlink to get the actual path (not through rust-default symlinks)
                let resolved = entry.path().canonicalize()?;
                return Ok(resolved.to_string_lossy().to_string());
            }
        }
    }
    anyhow::bail!(
        "lib{} rlib not found in {}",
        crate_name,
        sysroot_lib.display()
    );
}

pub(crate) fn shell_quote(s: &str) -> String {
    if s == "$out" || s.starts_with('$') {
        return s.to_string();
    }
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || "-_./+:,=@".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub(crate) fn sanitize_drv_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '+' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.starts_with('.') {
        format!("_{}", s)
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn shell_quote_plain() {
        assert_eq!(shell_quote("hello"), "hello");
        assert_eq!(shell_quote("/nix/store/abc-def"), "/nix/store/abc-def");
    }

    #[test]
    fn shell_quote_special() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn shell_quote_dollar_out() {
        assert_eq!(shell_quote("$out"), "$out");
        assert_eq!(shell_quote("$HOME"), "$HOME");
    }

    #[test]
    fn sanitize_drv_name_basic() {
        assert_eq!(sanitize_drv_name("hello-world_1.0"), "hello-world_1.0");
    }

    #[test]
    fn sanitize_drv_name_leading_dot() {
        assert_eq!(sanitize_drv_name(".hidden"), "_.hidden");
    }

    #[test]
    fn sanitize_drv_name_spaces() {
        assert_eq!(sanitize_drv_name("hello world!"), "hello-world-");
    }

    #[test]
    fn collect_store_paths_finds_paths() {
        let mut paths = HashSet::new();
        collect_store_paths(
            "something /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo and /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bar/bin",
            &mut paths,
        );
        assert!(paths.contains("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo"));
        assert!(paths.contains("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-bar"));
    }

    #[test]
    fn collect_store_paths_no_paths() {
        let mut paths = HashSet::new();
        collect_store_paths("no nix paths here", &mut paths);
        assert!(paths.is_empty());
    }

    #[test]
    fn collect_store_paths_multiple_in_string() {
        let mut paths = HashSet::new();
        let input = format!(
            "{} {} {}",
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-gcc",
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc/lib",
            "/nix/store/cccccccccccccccccccccccccccccccc-rust"
        );
        collect_store_paths(&input, &mut paths);
        assert_eq!(paths.len(), 3);
    }

    #[test]
    fn collect_store_paths_skips_invalid_base32() {
        let mut paths = HashSet::new();
        // 'e' is not in the Nix base32 alphabet — this matches clang wrapper placeholders
        collect_store_paths(
            "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-gcc-15.2.0",
            &mut paths,
        );
        assert!(paths.is_empty(), "placeholder path should be rejected");
    }

    #[test]
    fn collect_store_paths_mixed_valid_and_placeholder() {
        let mut paths = HashSet::new();
        let input = "-I/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-gcc-15.2.0/include \
                      -L/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc/lib";
        collect_store_paths(input, &mut paths);
        assert_eq!(paths.len(), 1);
        assert!(paths.contains("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-glibc"));
    }

    #[test]
    fn collect_store_paths_no_dash_at_32() {
        let mut paths = HashSet::new();
        collect_store_paths("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaafoo", &mut paths);
        assert!(paths.is_empty());
    }

    #[test]
    fn collect_store_paths_nested() {
        let mut paths = HashSet::new();
        collect_store_paths(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-outer/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-inner",
            &mut paths,
        );
        assert!(paths.contains("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-outer"));
        assert!(paths.contains("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-inner"));
    }

    #[test]
    fn collect_store_paths_at_string_boundary() {
        let mut paths = HashSet::new();
        collect_store_paths(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-start",
            &mut paths,
        );
        assert!(paths.contains("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-start"));
    }

    #[test]
    fn shell_quote_dollar_passthrough() {
        assert_eq!(shell_quote("$out"), "$out");
        assert_eq!(shell_quote("$out/bin"), "$out/bin");
    }

    proptest! {
        #[test]
        fn prop_shell_quote_deterministic(s in ".*") {
            prop_assert_eq!(shell_quote(&s), shell_quote(&s));
        }

        #[test]
        fn prop_shell_quote_dollar_passthrough(var in "[A-Z_][A-Z0-9_]{0,20}") {
            let input = format!("${}", var);
            prop_assert_eq!(shell_quote(&input), input);
        }

        #[test]
        fn prop_collect_store_paths_valid(
            hash in "[0-9a-z]{32}",
            name in "[a-z][a-z0-9-]{0,20}",
        ) {
            let input = format!("/nix/store/{}-{}", hash, name);
            let mut paths = HashSet::new();
            collect_store_paths(&input, &mut paths);
            for p in &paths {
                prop_assert!(p.starts_with("/nix/store/"));
                let after = p.strip_prefix("/nix/store/").unwrap();
                prop_assert!(after.len() >= 33);
                prop_assert_eq!(after.as_bytes()[32], b'-');
            }
        }

        #[test]
        fn prop_sanitize_drv_name_no_spaces(name in ".{1,50}") {
            let sanitized = sanitize_drv_name(&name);
            prop_assert!(!sanitized.contains(' '));
        }

        #[test]
        fn prop_sanitize_drv_name_no_leading_dot(name in ".{1,50}") {
            let sanitized = sanitize_drv_name(&name);
            prop_assert!(!sanitized.starts_with('.'));
        }

        #[test]
        fn prop_sanitize_drv_name_valid_chars(name in ".{1,50}") {
            let sanitized = sanitize_drv_name(&name);
            for c in sanitized.chars() {
                prop_assert!(
                    c.is_alphanumeric() || c == '.' || c == '_' || c == '-' || c == '+',
                    "invalid char: {:?}", c
                );
            }
        }
    }
}
