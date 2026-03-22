//! Cargo-style terminal output formatting.

use std::io::{self, IsTerminal, Write};

/// Print a cargo-style status message to stderr.
/// The label is right-aligned to 12 characters and printed in bold green.
pub fn status(label: &str, message: &str) {
    let stderr = io::stderr();
    let mut handle = stderr.lock();
    if stderr.is_terminal() {
        let _ = writeln!(handle, "\x1b[1;32m{:>12}\x1b[0m {}", label, message);
    } else {
        let _ = writeln!(handle, "{:>12} {}", label, message);
    }
}

/// Derivation kind parsed from a Nix store path.
#[derive(Debug, PartialEq)]
pub enum DrvKind {
    Compile,
    TestCompile,
    BuildScriptCompile,
    BuildScriptRun,
}

/// Parse a Nix derivation store path into (pkg_name, version, kind).
///
/// Our derivation names follow `{pkg}-{version}-{target}{suffix}.drv`
/// where suffix is `-build-script` or `-run-build-script`.
pub fn parse_drv_display(drv_path: &str) -> (String, String, DrvKind) {
    let basename = drv_path.strip_prefix("/nix/store/").unwrap_or(drv_path);

    // Skip 32-char hash + dash
    let name = if basename.len() > 33 && basename.as_bytes().get(32) == Some(&b'-') {
        &basename[33..]
    } else {
        basename
    };
    let name = name.strip_suffix(".drv").unwrap_or(name);

    // Detect and strip mode suffixes
    let (name, kind) = if let Some(n) = name.strip_suffix("-run-build-script") {
        (n, DrvKind::BuildScriptRun)
    } else if let Some(n) = name.strip_suffix("-build-script") {
        (n, DrvKind::BuildScriptCompile)
    } else if let Some(n) = name.strip_suffix("-test") {
        (n, DrvKind::TestCompile)
    } else {
        (name, DrvKind::Compile)
    };

    // Extract version: first dash-separated segment that starts with a digit
    // and contains a dot (e.g., "1.0.123")
    let parts: Vec<&str> = name.split('-').collect();
    let mut version_idx = None;
    for (i, part) in parts.iter().enumerate() {
        if i > 0 && part.chars().next().is_some_and(|c| c.is_ascii_digit()) && part.contains('.') {
            version_idx = Some(i);
            break;
        }
    }

    if let Some(vi) = version_idx {
        let pkg = parts[..vi].join("-");
        let version = parts[vi].to_string();
        (pkg, version, kind)
    } else {
        (name.to_string(), String::new(), kind)
    }
}

/// Parse a `building '/nix/store/hash-name.drv'...` line to extract the drv path.
pub fn parse_building_line(line: &str) -> Option<&str> {
    let line = line.trim();
    let rest = line.strip_prefix("building '")?;
    let end = rest.find('\'')?;
    let drv_path = &rest[..end];
    if drv_path.ends_with(".drv") {
        Some(drv_path)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_drv_display_compile() {
        let (pkg, ver, kind) = parse_drv_display(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-serde-1.0.210-serde-abcdef0123456789.drv",
        );
        assert_eq!(pkg, "serde");
        assert_eq!(ver, "1.0.210");
        assert_eq!(kind, DrvKind::Compile);
    }

    #[test]
    fn parse_drv_display_build_script_run() {
        let (pkg, ver, kind) = parse_drv_display(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-openssl-sys-0.9.104-build-script-run-build-script.drv",
        );
        assert_eq!(pkg, "openssl-sys");
        assert_eq!(ver, "0.9.104");
        assert_eq!(kind, DrvKind::BuildScriptRun);
    }

    #[test]
    fn parse_drv_display_build_script_compile() {
        let (pkg, ver, kind) = parse_drv_display(
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-cc-1.2.5-build-script-build-script.drv",
        );
        assert_eq!(pkg, "cc");
        assert_eq!(ver, "1.2.5");
        assert_eq!(kind, DrvKind::BuildScriptCompile);
    }

    #[test]
    fn parse_drv_display_no_version() {
        let (pkg, ver, kind) =
            parse_drv_display("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-my-crate-target.drv");
        assert_eq!(pkg, "my-crate-target");
        assert!(ver.is_empty());
        assert_eq!(kind, DrvKind::Compile);
    }

    #[test]
    fn parse_building_line_valid() {
        let line = "building '/nix/store/abc123-foo.drv'...";
        assert_eq!(parse_building_line(line), Some("/nix/store/abc123-foo.drv"));
    }

    #[test]
    fn parse_building_line_not_drv() {
        let line = "building '/nix/store/abc123-foo.tar.gz'...";
        assert_eq!(parse_building_line(line), None);
    }

    #[test]
    fn parse_building_line_no_match() {
        assert_eq!(parse_building_line("some other output"), None);
        assert_eq!(parse_building_line(""), None);
    }
}
