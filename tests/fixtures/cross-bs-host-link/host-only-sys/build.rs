use std::path::Path;
use std::process::Command;

// Compiles a host-format static archive and points the linker at it. This
// crate is reached only through a proc-macro, so cargo keeps the directives
// below inside the host compile kind. If cargo-schnee replays them on the
// cross link of `app` instead, the target linker is handed an x86_64 archive
// and rejects it.
fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dir = Path::new(&out_dir);
    let source = dir.join("hostonly.c");
    let object = dir.join("hostonly.o");
    let archive = dir.join("libhostonly.a");

    std::fs::write(&source, "int schnee_host_only(void) { return 42; }\n").unwrap();
    run(Command::new("cc")
        .arg("-c")
        .arg(&source)
        .arg("-o")
        .arg(&object));
    run(Command::new("ar").arg("rcs").arg(&archive).arg(&object));

    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=static=hostonly");
}

fn run(command: &mut Command) {
    let status = command.status().unwrap();
    assert!(status.success(), "{:?} failed with {}", command, status);
}
