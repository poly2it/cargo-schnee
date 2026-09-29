fn main() {
    println!("cargo:rerun-if-changed=../spec/api.txt");
    let spec = std::fs::read_to_string("../spec/api.txt").expect("reading the spec");
    println!("cargo:rustc-env=SPEC_LINE={}", spec.trim());
}
