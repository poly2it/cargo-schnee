//! In-process NAR serialization and store path computation.
//!
//! NAR (Nix ARchive) is a deterministic archive format used by Nix to hash
//! store paths. This module serializes a directory tree directly into NAR
//! format in-process, computes the store path from the NAR hash, and checks
//! existence to skip the `nix-store --add` subprocess on warm builds.

use crate::nix_encoding::{compress_hash, hex_lower, nix_base32_encode};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// NAR serialization
// ---------------------------------------------------------------------------

/// Serialize a file or directory to NAR format.
///
/// If `allowed_files` is Some, only files in the set are included (paths
/// relative to `root`). This enables .gitignore-aware filtering without
/// copying to a temp directory.
pub fn serialize_nar(root: &Path, allowed_files: Option<&HashSet<PathBuf>>) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024 * 1024);
    nar_string(&mut buf, "nix-archive-1");
    nar_serialize_path(&mut buf, root, root, allowed_files)?;
    Ok(buf)
}

fn nar_serialize_path(
    buf: &mut Vec<u8>,
    path: &Path,
    root: &Path,
    allowed_files: Option<&HashSet<PathBuf>>,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("Failed to stat {}", path.display()))?;

    nar_string(buf, "(");

    if meta.is_dir() {
        nar_string(buf, "type");
        nar_string(buf, "directory");

        // Entries must be sorted by name
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .with_context(|| format!("Failed to read dir {}", path.display()))?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let child_path = entry.path();

            // Skip symlinks (matching existing copy_dir_excluding behavior)
            let child_meta = std::fs::symlink_metadata(&child_path);
            if let Ok(ref m) = child_meta
                && m.file_type().is_symlink()
            {
                continue;
            }

            // If filtering, check if this subtree has any allowed files
            if let Some(allowed) = allowed_files {
                let rel = child_path.strip_prefix(root).unwrap_or(&child_path);
                let is_file = child_meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
                let is_dir = child_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                if is_file {
                    if !allowed.contains(rel) {
                        continue;
                    }
                } else if is_dir {
                    // Check if any allowed file has this directory as prefix
                    if !allowed.iter().any(|f| f.starts_with(rel)) {
                        continue;
                    }
                }
            }

            nar_string(buf, "entry");
            nar_string(buf, "(");
            nar_string(buf, "name");
            nar_string(buf, &name_str);
            nar_string(buf, "node");
            nar_serialize_path(buf, &child_path, root, allowed_files)?;
            nar_string(buf, ")");
        }
    } else if meta.is_file() {
        nar_string(buf, "type");
        nar_string(buf, "regular");

        // Check executable bit
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 != 0 {
                nar_string(buf, "executable");
                nar_string(buf, "");
            }
        }

        let contents =
            std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
        nar_string(buf, "contents");
        nar_bytes(buf, &contents);
    } else if meta.file_type().is_symlink() {
        nar_string(buf, "type");
        nar_string(buf, "symlink");
        let target = std::fs::read_link(path)?;
        nar_string(buf, "target");
        nar_string(buf, &target.to_string_lossy());
    } else {
        anyhow::bail!("Unsupported file type at {}", path.display());
    }

    nar_string(buf, ")");
    Ok(())
}

/// Write a NAR string: u64 length + bytes + zero-padding to 8-byte boundary.
fn nar_string(buf: &mut Vec<u8>, s: &str) {
    nar_bytes(buf, s.as_bytes());
}

fn nar_bytes(buf: &mut Vec<u8>, data: &[u8]) {
    buf.extend_from_slice(&(data.len() as u64).to_le_bytes());
    buf.extend_from_slice(data);
    let padding = (8 - (data.len() % 8)) % 8;
    buf.extend_from_slice(&[0u8; 8][..padding]);
}

// ---------------------------------------------------------------------------
// Compute store path for a NAR-hashed path (like nix-store --add)
// ---------------------------------------------------------------------------

/// Compute the store path for a NAR-hashed source addition.
///
/// This mirrors `nix-store --add` path computation:
///   nar_hash = sha256(nar_bytes)
///   inner = "source:sha256:<hex(nar_hash)>:/nix/store:<name>"
///   outer = sha256(inner)
///   path = "/nix/store/" + nix_base32(compress(outer, 20)) + "-" + name
pub fn compute_nar_store_path(name: &str, nar: &[u8]) -> String {
    let nar_hash = Sha256::digest(nar);
    let fingerprint = format!("source:sha256:{}:/nix/store:{}", hex_lower(&nar_hash), name,);
    let outer = Sha256::digest(fingerprint.as_bytes());
    let compressed = compress_hash(&outer, 20);
    format!("/nix/store/{}-{}", nix_base32_encode(&compressed), name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nix_encoding::NIX_BASE32;

    #[test]
    fn nar_string_length_and_padding() {
        let mut buf = Vec::new();
        nar_string(&mut buf, "abc");
        // 8 bytes length (3 as u64le) + 3 bytes data + 5 bytes padding = 16
        assert_eq!(buf.len(), 16);
        assert_eq!(&buf[..8], &3u64.to_le_bytes());
        assert_eq!(&buf[8..11], b"abc");
        assert_eq!(&buf[11..16], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn nar_string_aligned() {
        let mut buf = Vec::new();
        nar_string(&mut buf, "abcdefgh"); // exactly 8 bytes, no padding needed
        assert_eq!(buf.len(), 16);
        assert_eq!(&buf[8..16], b"abcdefgh");
    }

    #[test]
    fn nar_string_empty() {
        let mut buf = Vec::new();
        nar_string(&mut buf, "");
        assert_eq!(buf.len(), 8); // just the length
        assert_eq!(&buf[..8], &0u64.to_le_bytes());
    }

    #[test]
    fn compute_nar_store_path_deterministic() {
        let nar = b"test nar content";
        let p1 = compute_nar_store_path("test-name", nar);
        let p2 = compute_nar_store_path("test-name", nar);
        assert_eq!(p1, p2);
    }

    #[test]
    fn compute_nar_store_path_format() {
        let nar = b"some nar data";
        let path = compute_nar_store_path("my-source", nar);
        assert!(path.starts_with("/nix/store/"));
        assert!(path.ends_with("-my-source"));
        // Hash part is 32 chars in nix base32
        let after_store = path.strip_prefix("/nix/store/").unwrap();
        let hash_part = &after_store[..32];
        assert_eq!(hash_part.len(), 32);
        assert!(hash_part.bytes().all(|b| NIX_BASE32.contains(&b)));
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_nar_store_path_format(
            name in "[a-z][a-z0-9-]{0,20}",
            content in proptest::collection::vec(any::<u8>(), 1..200),
        ) {
            let path = compute_nar_store_path(&name, &content);
            prop_assert!(path.starts_with("/nix/store/"));
            let after_store = path.strip_prefix("/nix/store/").unwrap();
            let hash_part = &after_store[..32];
            prop_assert_eq!(hash_part.len(), 32);
            prop_assert!(hash_part.bytes().all(|b| NIX_BASE32.contains(&b)));
        }

        #[test]
        fn prop_nar_store_path_deterministic(
            name in "[a-z][a-z0-9-]{0,20}",
            content in proptest::collection::vec(any::<u8>(), 1..200),
        ) {
            let p1 = compute_nar_store_path(&name, &content);
            let p2 = compute_nar_store_path(&name, &content);
            prop_assert_eq!(p1, p2);
        }
    }
}
