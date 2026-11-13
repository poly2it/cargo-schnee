fn main() {
    let root = std::env::var("DEP_PROBE_ROOT").expect("DEP_PROBE_ROOT is not set");
    println!("cargo::rustc-env=PROBE_ROOT={root}");
}
