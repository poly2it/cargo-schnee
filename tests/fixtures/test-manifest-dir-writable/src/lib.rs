/// Verify CARGO_MANIFEST_DIR behaviour in cargo-schnee.
///
/// Both compile-time `env!("CARGO_MANIFEST_DIR")` and runtime
/// `std::env::var("CARGO_MANIFEST_DIR")` should point to the original
/// writable project directory, so tests that write generated files
/// (e.g. TypeScript bindings) relative to CARGO_MANIFEST_DIR work.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn compile_time_manifest_dir_is_readable() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(dir.is_dir(), "compile-time CARGO_MANIFEST_DIR should exist");
        let cargo_toml = dir.join("Cargo.toml");
        assert!(
            cargo_toml.is_file(),
            "Cargo.toml should be readable at compile-time CARGO_MANIFEST_DIR"
        );
    }

    /// Regression test: compile-time `env!("CARGO_MANIFEST_DIR")` must point
    /// to a writable path so tests that generate files (e.g. yerpc TS bindings)
    /// can write relative to it.
    #[test]
    fn write_to_manifest_dir_compile_time() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-output-compile-time");
        std::fs::create_dir_all(&dir).expect("create_dir_all failed");
        let out = dir.join("compile_time.txt");
        std::fs::write(&out, "ok").expect("write failed");
        assert!(out.exists());
        // Clean up
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_dir(&dir);
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

    /// Compile-time and runtime CARGO_MANIFEST_DIR should agree.
    #[test]
    fn compile_time_and_runtime_manifest_dir_agree() {
        let compile_time = env!("CARGO_MANIFEST_DIR");
        let runtime =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set at runtime");
        assert_eq!(
            compile_time, runtime,
            "compile-time and runtime CARGO_MANIFEST_DIR should match"
        );
    }
}
