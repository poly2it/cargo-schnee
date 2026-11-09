use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn run(program: &str, args: &[String]) {
    let status = Command::new(program).args(args).status().unwrap();
    assert!(status.success(), "{program} {args:?} failed");
}

fn main() {
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let out_str = out.to_string_lossy().to_string();
    fs::write(
        out.join("generated.rs"),
        "pub const GENERATED_FILE: &str = file!();\n",
    )
    .unwrap();

    // Generated C with debug info, the way the cmake crate builds inside
    // `OUT_DIR`, so its object names its own path and the libc headers.
    fs::write(
        out.join("native.c"),
        "#include <stdio.h>\nint native_answer(void) { return (int)sizeof(FILE) > 0 ? 42 : 0; }\n",
    )
    .unwrap();
    let mut args: Vec<String> = ["-c", "-g", "-fPIC"].map(String::from).to_vec();
    args.extend(env::var("CFLAGS").unwrap_or_default().split_whitespace().map(String::from));
    args.extend([format!("{out_str}/native.c"), "-o".into(), format!("{out_str}/native.o")]);
    run("cc", &args);
    run(
        "ar",
        &["rcs".into(), format!("{out_str}/libnative.a"), format!("{out_str}/native.o")],
    );
    println!("cargo:rustc-link-search=native={out_str}");
    println!("cargo:rustc-link-lib=static=native");
}
