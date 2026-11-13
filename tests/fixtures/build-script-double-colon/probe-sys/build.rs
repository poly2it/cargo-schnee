fn main() {
    println!("cargo::rustc-check-cfg=cfg(probe_cfg)");
    println!("cargo::rustc-cfg=probe_cfg");
    println!("cargo::rustc-env=PROBE_ENV=from-env");
    println!("cargo::metadata=root=from-metadata");
}
