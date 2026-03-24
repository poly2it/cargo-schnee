use std::fs;
use std::path::PathBuf;

fn main() {
    // Simulate crates that write temp data under $HOME (e.g. embedded DB engines).
    // In a Nix sandbox, $HOME is typically /homeless-shelter which doesn't exist.
    // cargo-schnee sets HOME=$TMPDIR so this succeeds.
    let home = std::env::var("HOME").expect("HOME not set");
    let home = PathBuf::from(home);
    let scratch = home.join("build-script-scratch");
    fs::create_dir_all(&scratch)
        .unwrap_or_else(|e| panic!("create dir {}: {}", scratch.display(), e));
    let marker = scratch.join("marker.txt");
    fs::write(&marker, "ok").unwrap_or_else(|e| panic!("write {}: {}", marker.display(), e));
}
