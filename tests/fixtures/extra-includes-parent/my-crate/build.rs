use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let spec_path = Path::new(&manifest_dir).join("../../extra-includes-parent-data/spec.json");
    let spec = std::fs::read_to_string(&spec_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", spec_path.display(), e));
    assert!(spec.contains("hello from parent data"));
    println!("cargo:rustc-env=SPEC_CONTENT={}", spec.trim());
}
