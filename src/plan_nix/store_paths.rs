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
    let mut conn = NixDaemonConn::connect().context("Connecting to the Nix daemon")?;
    register_gc_root_over(&mut conn, link, name, paths)
}

/// [`register_gc_root`] over an open connection. The connection holds a
/// temporary root on the text path until it closes, so the path survives
/// until `link` roots it.
fn register_gc_root_over(
    conn: &mut NixDaemonConn,
    link: &Path,
    name: &str,
    paths: &[String],
) -> Result<String> {
    let mut refs: Vec<&str> = paths.iter().map(String::as_str).collect();
    refs.sort_unstable();
    refs.dedup();
    let content = refs.join("\n");
    let root_path = conn
        .add_text_to_store(name, content.as_bytes(), &refs)
        .context("Registering the GC root store path")?;

    let dir = link.parent().context("GC root link has no parent")?;
    std::fs::create_dir_all(dir)?;
    // Renaming a fresh symlink over the old one replaces the root in one
    // step, so a collection never sees the link missing.
    let tmp = dir.join(format!(
        ".{}.{}",
        link.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&root_path, &tmp)
        .with_context(|| format!("Creating {}", tmp.display()))?;
    std::fs::rename(&tmp, link).with_context(|| format!("Replacing {}", link.display()))?;
    let absolute = std::path::absolute(link)?;
    conn.add_indirect_root(&absolute.to_string_lossy())
        .context("Registering the indirect GC root")?;
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

    /// The root must reach the daemon as `wopAddIndirectRoot` over the
    /// connection, since a `nix-store` spawn would trip the check that a
    /// warm build realises nothing.
    #[test]
    fn registers_the_root_over_the_daemon_connection() {
        use crate::plan_nix::daemon::fake::{self, Request};
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("target/.schnee-roots/dev-x-build");
        let paths = vec![
            "/nix/store/00000000000000000000000000000000-app".to_string(),
            "/nix/store/11111111111111111111111111111111-app.drv".to_string(),
        ];
        let (mut conn, handle) = fake::connect(HashSet::new());
        let root = register_gc_root_over(&mut conn, &link, "app-build-gc-root", &paths).unwrap();
        drop(conn);

        assert_eq!(std::fs::read_link(&link).unwrap().to_string_lossy(), root);
        assert_eq!(
            handle.join().unwrap(),
            vec![
                Request::AddTextToStore {
                    name: "app-build-gc-root".into(),
                    references: paths,
                },
                Request::AddIndirectRoot(link.to_string_lossy().into_owned()),
            ]
        );
    }
}
