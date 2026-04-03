fn main() {
    let flag = std::env::var("MY_CUSTOM_FLAG")
        .unwrap_or_else(|_| panic!("MY_CUSTOM_FLAG not set in build script"));
    println!("cargo:rustc-env=DEP_FLAG={}", flag);
}
