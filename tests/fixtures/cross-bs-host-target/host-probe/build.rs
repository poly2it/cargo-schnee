fn main() {
    let host = std::env::var("HOST").unwrap();
    let target = std::env::var("TARGET").unwrap();
    // Emit both values so the proc-macro can verify correctness at compile time.
    // When compiled for the host (via proc-macro dep chain), TARGET must equal HOST.
    // When compiled for the target (cross), TARGET equals the cross triple.
    // The proc-macro only links against the host variant, so it will always see
    // BS_TARGET == HOST — if not, cargo-schnee has a bug.
    println!("cargo:rustc-env=BS_TARGET={}", target);
    println!("cargo:rustc-env=BS_HOST={}", host);
}
