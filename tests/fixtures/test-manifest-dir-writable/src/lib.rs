/// Write a file relative to CARGO_MANIFEST_DIR.
///
/// Uses the compile-time `env!("CARGO_MANIFEST_DIR")` value, which in
/// cargo-schnee is set during per-crate derivation compilation.  The path
/// must resolve to a writable location when the test binary runs.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn write_to_manifest_dir_compile_time() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-output");
        std::fs::create_dir_all(&dir).expect("create_dir_all failed");
        let out = dir.join("compile_time.txt");
        std::fs::write(&out, "ok").expect("write failed");
        assert!(out.exists());
        // Clean up
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn write_to_manifest_dir_runtime() {
        let dir = PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set at runtime"),
        )
        .join("target/test-output");
        std::fs::create_dir_all(&dir).expect("create_dir_all failed");
        let out = dir.join("runtime.txt");
        std::fs::write(&out, "ok").expect("write failed");
        assert!(out.exists());
        // Clean up
        let _ = std::fs::remove_file(&out);
    }
}
