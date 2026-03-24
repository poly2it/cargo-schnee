use std::fs;
use std::path::{Path, PathBuf};

/// Recursive directory copy that preserves source permissions.
/// This mirrors what crates like pglite-oxide do internally — they copy
/// assets from their source tree into a temp dir. When the source is in
/// the read-only Nix store, directories have 555 permissions which get
/// preserved, making the destination dirs read-only.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    // Mimic permission-preserving copy: set dst dir perms to match source
    let src_perms = fs::metadata(src)?.permissions();
    fs::set_permissions(dst, src_perms)?;

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
