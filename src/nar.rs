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
    nar_serialize_path(&mut buf, root, root, allowed_files, false)?;
    Ok(buf)
}

/// Serialize a tree to NAR, but blank the contents of every file except
/// `Cargo.toml`/`Cargo.lock`. The result is a function of the manifest contents
/// and the *set* of source paths, not of source file *bodies* — exactly the
/// input the unit-graph planner depends on. Building the planner against this
/// skeleton means a `.rs` body edit does not move the planner's input (so it
/// neither re-runs nor re-emits the unit-drv set), while adding/removing a
/// source file or editing a manifest does.
pub fn serialize_nar_skeleton(
    root: &Path,
    allowed_files: Option<&HashSet<PathBuf>>,
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024 * 1024);
    nar_string(&mut buf, "nix-archive-1");
    nar_serialize_path(&mut buf, root, root, allowed_files, true)?;
    Ok(buf)
}

/// A manifest whose contents the planner actually reads; kept verbatim in the
/// skeleton. Everything else is blanked.
fn is_manifest(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("Cargo.toml") | Some("Cargo.lock")
    )
}

fn nar_serialize_path(
    buf: &mut Vec<u8>,
    path: &Path,
    root: &Path,
    allowed_files: Option<&HashSet<PathBuf>>,
    skeleton: bool,
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
            nar_serialize_path(buf, &child_path, root, allowed_files, skeleton)?;
            nar_string(buf, ")");
        }
    } else if meta.is_file() {
        nar_string(buf, "type");
        nar_string(buf, "regular");

        // In skeleton mode, blank every non-manifest file: the planner only
        // reads manifests and the source-path set, never source bodies, so the
        // skeleton's hash is body-independent.
        let blank = skeleton && !is_manifest(path);

        // Check executable bit (blanked files are plain regular files).
        #[cfg(unix)]
        if !blank {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 != 0 {
                nar_string(buf, "executable");
                nar_string(buf, "");
            }
        }

        let contents = if blank {
            Vec::new()
        } else {
            std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?
        };
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

// ---------------------------------------------------------------------------
// Per-crate source slicing
// ---------------------------------------------------------------------------

/// Compute the content-addressed store path for a single crate's source
/// subtree, independent of any other crate's contents.
///
/// `crate_rel` is the crate directory relative to `project_dir`; `allowed_files`
/// are project-relative paths (as produced by git collection). The NAR is rooted
/// at the crate directory, so the result is a function of *only* that crate's
/// files: editing a file outside `crate_rel` cannot change it. This is the
/// per-unit source input that decouples the derivation graph — replacing the
/// single whole-tree `project-src` NAR that every unit otherwise shares.
pub fn crate_source_store_path(
    project_dir: &Path,
    crate_rel: &Path,
    allowed_files: &HashSet<PathBuf>,
) -> Result<String> {
    let crate_dir = project_dir.join(crate_rel);
    // Re-base the crate's files relative to the crate directory (the NAR root).
    let crate_files: HashSet<PathBuf> = allowed_files
        .iter()
        .filter_map(|f| f.strip_prefix(crate_rel).ok().map(Path::to_path_buf))
        .collect();
    let nar = serialize_nar(&crate_dir, Some(&crate_files))?;
    let name = format!(
        "{}-src",
        crate_rel
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("crate")
    );
    Ok(compute_nar_store_path(&name, &nar))
}

/// Content-addressed store path of the planner's *skeleton* source: manifests
/// verbatim, every other file blanked (see `serialize_nar_skeleton`). Stable
/// across source body edits; moves only on a manifest change or a change to the
/// set of source paths — the conditions under which the unit graph can differ.
pub fn skeleton_source_store_path(
    project_dir: &Path,
    allowed_files: &HashSet<PathBuf>,
) -> Result<String> {
    let nar = serialize_nar_skeleton(project_dir, Some(allowed_files))?;
    Ok(compute_nar_store_path("project-src-skeleton", &nar))
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

    fn write(p: &Path, s: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    }

    /// The core decoupling invariant: a crate's per-crate source path is a
    /// function of only its own files, so editing a sibling crate leaves it
    /// byte-identical — whereas the whole-tree NAR (today's single
    /// `project-src`) moves on any edit, coupling every unit.
    #[test]
    fn per_crate_source_path_independent_of_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\n",
        );
        write(&root.join("crate-a/src/lib.rs"), "pub fn a() {}\n");
        write(
            &root.join("crate-b/Cargo.toml"),
            "[package]\nname = \"crate-b\"\n",
        );
        write(&root.join("crate-b/src/lib.rs"), "pub fn b() {}\n");

        let allowed: HashSet<PathBuf> = [
            "crate-a/Cargo.toml",
            "crate-a/src/lib.rs",
            "crate-b/Cargo.toml",
            "crate-b/src/lib.rs",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        let a1 = crate_source_store_path(root, Path::new("crate-a"), &allowed).unwrap();
        let b1 = crate_source_store_path(root, Path::new("crate-b"), &allowed).unwrap();
        let whole1 =
            compute_nar_store_path("project-src", &serialize_nar(root, Some(&allowed)).unwrap());

        // Edit ONLY crate-b.
        write(
            &root.join("crate-b/src/lib.rs"),
            "pub fn b() { /* changed */ }\n",
        );

        let a2 = crate_source_store_path(root, Path::new("crate-a"), &allowed).unwrap();
        let b2 = crate_source_store_path(root, Path::new("crate-b"), &allowed).unwrap();
        let whole2 =
            compute_nar_store_path("project-src", &serialize_nar(root, Some(&allowed)).unwrap());

        assert_eq!(
            a1, a2,
            "editing crate-b must NOT change crate-a's per-crate source path"
        );
        assert_ne!(
            b1, b2,
            "editing crate-b must change crate-b's per-crate source path"
        );
        assert_ne!(
            whole1, whole2,
            "the whole-tree NAR couples all crates: it moves on any edit (the bug being fixed)"
        );
    }

    /// Change 4: the planner skeleton is body-independent, so a `.rs` body edit
    /// does not move the planner's input; a manifest edit or a source-file
    /// add/remove does (those can change the unit graph).
    #[test]
    fn skeleton_source_path_is_body_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write(
            &root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crate-a\"]\n",
        );
        write(&root.join("Cargo.lock"), "# lock\n");
        write(
            &root.join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\n",
        );
        write(&root.join("crate-a/src/lib.rs"), "pub fn a() {}\n");

        let allowed: HashSet<PathBuf> = [
            "Cargo.toml",
            "Cargo.lock",
            "crate-a/Cargo.toml",
            "crate-a/src/lib.rs",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        let s1 = skeleton_source_store_path(root, &allowed).unwrap();

        // A source BODY edit must not move the skeleton.
        write(
            &root.join("crate-a/src/lib.rs"),
            "pub fn a() { let _ = 1; }\n",
        );
        let s2 = skeleton_source_store_path(root, &allowed).unwrap();
        assert_eq!(s1, s2, "a .rs body edit must NOT move the planner skeleton");

        // A manifest edit must move it.
        write(
            &root.join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\nedition = \"2021\"\n",
        );
        let s3 = skeleton_source_store_path(root, &allowed).unwrap();
        assert_ne!(s2, s3, "a manifest edit must move the planner skeleton");

        // Adding a source path must move it (autodiscovered targets depend on file presence).
        write(&root.join("crate-a/src/bin/extra.rs"), "fn main() {}\n");
        let mut allowed2 = allowed.clone();
        allowed2.insert(PathBuf::from("crate-a/src/bin/extra.rs"));
        let s4 = skeleton_source_store_path(root, &allowed2).unwrap();
        assert_ne!(
            s3, s4,
            "adding a source file must move the planner skeleton"
        );
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
