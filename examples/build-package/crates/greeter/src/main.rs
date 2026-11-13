use serde::Serialize;

#[derive(Serialize)]
struct Greeting {
    message: String,
}

fn main() {
    let greeting = Greeting {
        message: "hello from cargo-schnee buildPackage".to_string(),
    };
    println!("{}", serde_json::to_string_pretty(&greeting).unwrap());
}

#[cfg(test)]
mod tests {
    /// `lib.testPackage` gives the test binary its source tree as
    /// `CARGO_MANIFEST_DIR`, so fixtures resolve relative to it.
    #[test]
    fn manifest_dir_holds_the_sources() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(dir.join("Cargo.toml").is_file(), "{}", dir.display());
    }
}
