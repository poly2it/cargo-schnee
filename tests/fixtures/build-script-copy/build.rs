use std::fs;
use std::path::{Path, PathBuf};

/// Recursive directory copy using fs::copy (preserves source permissions).
/// fs::copy on Linux calls fchmod(dst_fd, src_mode) after writing, setting
/// files from the Nix store to 444 (read-only). If multiple threads copy
/// to the same location, the second thread can't overwrite a 444 file.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
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
const BAKED_MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn main() {
    let assets_src = PathBuf::from(BAKED_MANIFEST_DIR).join("assets");

    if !assets_src.exists() {
        return;
    }

    let tmp = PathBuf::from("tmp").join("copied-assets");

    // Copy TWICE to the same location — the second copy must succeed.
    // This simulates multi-threaded code where multiple threads copy to the
    // same destination. fs::copy sets destination files to 444 (source perms),
    // so the second copy would fail without the LD_PRELOAD shim ensuring
    // owner-write permission on all files.
    copy_dir_all(&assets_src, &tmp).unwrap_or_else(|e| panic!("first copy: {}", e));
    copy_dir_all(&assets_src, &tmp).unwrap_or_else(|e| panic!("second copy (overwrite): {}", e));

    let output = tmp.join("bin").join("output.txt");
    fs::write(&output, "generated").unwrap_or_else(|e| panic!("write {}: {}", output.display(), e));
}
