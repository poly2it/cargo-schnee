#[cfg(test)]
mod tests {
    #[test]
    fn working_dir_is_crate_dir() {
        let cwd = std::env::current_dir().unwrap();
        let manifest_dir = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"),
        );
        // Canonicalize both to resolve symlinks
        let cwd = std::fs::canonicalize(&cwd).unwrap();
        let manifest_dir = std::fs::canonicalize(&manifest_dir).unwrap();
        assert_eq!(
            cwd,
            manifest_dir,
            "working directory ({}) should match CARGO_MANIFEST_DIR ({})",
            cwd.display(),
            manifest_dir.display()
        );
    }
}
