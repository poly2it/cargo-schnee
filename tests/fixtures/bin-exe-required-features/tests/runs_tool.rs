#[test]
fn runs_the_package_binary() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bin-exe-tool"))
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "tool-ok\n");
}
