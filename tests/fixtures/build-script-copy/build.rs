use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Recursive directory copy that preserves source permissions via mkdir mode.
/// Some crates use DirBuilder::mode() to create destination directories with
/// the source directory's permission bits, bypassing chmod entirely.
/// When the source is in the read-only Nix store, directories have 555
/// permissions which get applied to mkdir, making the destination dirs
/// read-only.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    let src_perms = fs::metadata(src)?.permissions();
    // Create directory with source permissions via mkdir(path, mode)
    // This bypasses chmod — the permissions are set at creation time.
    fs::DirBuilder::new()
        .mode(src_perms.mode())
        .recursive(true)
        .create(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Bake in our own CARGO_MANIFEST_DIR at compile time.
/// This simulates how dependency crates (e.g. pglite-oxide) use
/// env!("CARGO_MANIFEST_DIR") to find their assets at build time.
/// When compiled under cargo-schnee, this path points to the Nix store
/// (read-only, 555 directory permissions).
const BAKED_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn main() {
    let assets_src = PathBuf::from(BAKED_MANIFEST_DIR).join("assets");

    if !assets_src.exists() {
        // Not in a Nix build or assets not available — skip
        return;
    }

    // Copy assets to a relative path from CWD (like pglite does)
    let tmp = PathBuf::from("tmp").join("copied-assets");
    copy_dir_all(&assets_src, &tmp)
        .unwrap_or_else(|e| panic!("copy assets to {}: {}", tmp.display(), e));

    // After the copy, try writing a new file into the copied directory.
    // This fails if directory permissions were preserved from the Nix store (555)
    // and the LD_PRELOAD shim isn't active.
    let output = tmp.join("bin").join("output.txt");
    fs::write(&output, "generated").unwrap_or_else(|e| panic!("write {}: {}", output.display(), e));
}
