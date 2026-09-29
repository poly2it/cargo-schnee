//! The harness starts two `cargo schnee test` runs of this crate from two
//! checkouts. Each test announces itself with `started` in its working
//! directory, which is its own checkout, and waits for `release` until the
//! other run has started too. Only then does it look at the compile-time
//! `CARGO_MANIFEST_DIR`, which must still be its own checkout.

use std::path::Path;
use std::time::{Duration, Instant};

#[test]
fn compile_time_manifest_dir_is_this_checkout() {
    let own = std::env::current_dir().unwrap().canonicalize().unwrap();
    std::fs::write(own.join("started"), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(600);
    while !own.join("release").exists() {
        assert!(Instant::now() < deadline, "never released");
        std::thread::sleep(Duration::from_millis(50));
    }
    let baked = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize().unwrap();
    assert_eq!(baked, own);
    std::fs::write(Path::new(env!("CARGO_MANIFEST_DIR")).join("target/written"), "").unwrap();
    assert!(own.join("target/written").exists());
}
