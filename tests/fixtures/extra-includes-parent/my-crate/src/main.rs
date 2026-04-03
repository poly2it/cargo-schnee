fn main() {
    let spec = env!("SPEC_CONTENT");
    println!("{}", spec);
    // Use anyhow to ensure the vendored dep (which has a build script) works
    let _: anyhow::Result<()> = Ok(());
}
