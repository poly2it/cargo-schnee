//! Validity checks and garbage-collector roots for the store paths that
//! a warm build reuses.
//!
//! A collected path may still exist on disk while the store database no
//! longer lists it, so validity is always asked of the store, never of
//! the filesystem.

use super::daemon::NixDaemonConn;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

/// The subset of `paths` that the store considers valid.
///
/// Asks the daemon in one round trip and falls back to
/// `nix-store --check-validity` when no daemon is reachable, which is the
/// case for a single-user installation.
pub(crate) fn valid_store_paths(paths: &[&str]) -> Result<HashSet<String>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    if let Ok(mut conn) = NixDaemonConn::connect() {
        return conn.query_valid_paths(paths);
    }
    let output = Command::new("nix-store")
        .arg("--check-validity")
        .arg("--print-invalid")
        .args(paths)
        .output()
        .context("Failed to run nix-store --check-validity")?;
    if !output.status.success() {
        anyhow::bail!(
            "nix-store --check-validity failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8(output.stdout)?;
    let invalid: HashSet<&str> = stdout.lines().map(str::trim).collect();
    Ok(paths
        .iter()
        .filter(|p| !invalid.contains(**p))
        .map(|p| p.to_string())
        .collect())
}

/// Whether the store considers `path` valid.
pub(crate) fn is_valid_store_path(path: &str) -> Result<bool> {
    Ok(valid_store_paths(&[path])?.contains(path))
}

/// The top-level store path that `path` lies in, such as
/// `/nix/store/<hash>-vendor` for `/nix/store/<hash>-vendor/serde/src/lib.rs`.
/// `None` when `path` is not inside `store_dir`.
pub(crate) fn store_path_of<'a>(path: &'a str, store_dir: &str) -> Option<&'a str> {
    let rest = path.strip_prefix(store_dir)?.strip_prefix('/')?;
    let name_len = rest.find('/').unwrap_or(rest.len());
    if name_len == 0 {
        return None;
    }
    Some(&path[..store_dir.len() + 1 + name_len])
}

/// The store directory that store paths in this process live under.
pub(crate) fn store_dir() -> String {
    std::env::var("NIX_STORE_DIR").unwrap_or_else(|_| "/nix/store".into())
}

/// The output paths of the derivations in `drvs`, as a set.
///
/// Content-addressed outputs have no path until they are built, so this
/// asks the store for the realisation of each `<drv>^out`. One
/// `nix path-info` call covers the whole list.
pub(crate) fn realised_outputs(drvs: &[String]) -> Result<Vec<String>> {
    if drvs.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("nix")
        .args([
            "path-info",
            "--extra-experimental-features",
            "nix-command ca-derivations dynamic-derivations",
            "--stdin",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn nix path-info")?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().context("stdin not piped")?;
        for drv in drvs {
            writeln!(stdin, "{drv}^out")?;
        }
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "nix path-info failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// The references of the store path that `link` points at, or `None`
/// when `link` does not point at a valid store path.
pub(crate) fn references_of_link(link: &Path) -> Option<HashSet<String>> {
    let target = std::fs::read_link(link).ok()?;
    let mut conn = NixDaemonConn::connect().ok()?;
    let refs = conn.query_path_references(&target.to_string_lossy()).ok()?;
    Some(refs.into_iter().collect())
}

/// Keep `paths` and everything they reference alive through the garbage
/// collector, as long as `link` exists.
///
/// Registers a text store path named `name` that references every entry
/// of `paths`, points `link` at it and registers `link` as an indirect
/// root. A later call with the same `link` replaces the previous root, so
/// what it kept alive becomes collectable again. Returns the text store
/// path.
pub(crate) fn register_gc_root(link: &Path, name: &str, paths: &[String]) -> Result<String> {
    let mut refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    refs.sort_unstable();
    refs.dedup();
    let content = refs.join("\n");

    // The connection holds a temporary root on the text path until it
    // closes, so the path survives until `link` roots it.
    let mut conn = NixDaemonConn::connect().context("Connecting to the Nix daemon")?;
    let root_path = conn
        .add_text_to_store(name, content.as_bytes(), &refs)
        .context("Registering the GC root store path")?;

    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let output = Command::new("nix-store")
        .arg("--add-root")
        .arg(link)
        .arg("--realise")
        .arg(&root_path)
        .output()
        .context("Failed to run nix-store --add-root")?;
    if !output.status.success() {
        anyhow::bail!(
            "nix-store --add-root failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    drop(conn);
    Ok(root_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_path_of_strips_the_path_inside_the_store_path() {
        assert_eq!(
            store_path_of(
                "/nix/store/00000000000000000000000000000000-vendor/serde/src/lib.rs",
                "/nix/store"
            ),
            Some("/nix/store/00000000000000000000000000000000-vendor")
        );
        assert_eq!(
            store_path_of(
                "/nix/store/00000000000000000000000000000000-app",
                "/nix/store"
            ),
            Some("/nix/store/00000000000000000000000000000000-app")
        );
    }

    #[test]
    fn store_path_of_rejects_paths_outside_the_store() {
        assert_eq!(store_path_of("/home/user/project/src", "/nix/store"), None);
        assert_eq!(store_path_of("/nix/storefront/x", "/nix/store"), None);
        assert_eq!(store_path_of("/nix/store/", "/nix/store"), None);
        assert_eq!(store_path_of("/nix/store", "/nix/store"), None);
    }
}
