/// Verify CARGO_MANIFEST_DIR behaviour in cargo-schnee.
///
/// Compile-time `env!("CARGO_MANIFEST_DIR")` points to the Nix store source,
/// which is readable (so proc macros that resolve files at compile time work).
/// Runtime `std::env::var("CARGO_MANIFEST_DIR")` points to the original
/// project directory, which is writable.
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
