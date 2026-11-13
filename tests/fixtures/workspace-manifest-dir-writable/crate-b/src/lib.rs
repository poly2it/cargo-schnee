/// Verify CARGO_MANIFEST_DIR behaviour for a workspace member.
///
/// A member crate is sliced to its own per-crate source store, so its
/// manifest dir no longer sits under the project source store. The runner
/// must still point the member's symlink at the checkout, and each member
/// must get its own symlink rather than sharing one.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn compile_time_and_runtime_manifest_dir_agree() {
        let compile_time = env!("CARGO_MANIFEST_DIR");
        let runtime =
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set at runtime");
        assert_eq!(compile_time, runtime);
    }

    #[test]
    fn manifest_dir_names_this_crate() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).expect("read Cargo.toml");
        assert!(
            manifest.contains("name = \"crate-b\""),
            "CARGO_MANIFEST_DIR resolved to the wrong crate: {}",
            manifest
        );
    }

    #[test]
    fn manifest_dir_is_writable() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-output");
        std::fs::create_dir_all(&dir).expect("create_dir_all failed");
        let out = dir.join("crate-b.txt");
        std::fs::write(&out, "ok").expect("write failed");
        let _ = std::fs::remove_file(&out);
    }
}
