fn main() {
    let flag = std::env::var("MY_CUSTOM_FLAG")
        .unwrap_or_else(|_| panic!("MY_CUSTOM_FLAG not set in build script"));
    println!("cargo:rustc-env=DEP_FLAG={}", flag);
    let hyphenated = std::env::var("MY-HYPHEN-FLAG")
        .unwrap_or_else(|_| panic!("MY-HYPHEN-FLAG not set in build script"));
    println!("cargo:rustc-env=DEP_HYPHEN_FLAG={}", hyphenated);
}
