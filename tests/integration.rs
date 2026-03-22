//! Integration tests for cargo-schnee.
//!
//! These tests require a running Nix daemon with ca-derivations enabled.
//! Run with: `cargo test -- --ignored`

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Get the path to the cargo-schnee binary built by cargo test.
fn cargo_schnee_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-schnee"))
}

/// Run `cargo-schnee build` on the given manifest path and assert success.
/// Returns (stdout, stderr) on success.
fn run_schnee_build(manifest_path: &Path) -> (String, String) {
    let output = Command::new(cargo_schnee_bin())
        .arg("schnee")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest_path)
        .output()
        .expect("Failed to execute cargo-schnee");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "cargo-schnee build failed for {}:\nstdout:\n{}\nstderr:\n{}",
        manifest_path.display(),
        stdout,
        stderr,
    );

    (stdout, stderr)
}

/// Ensure a git repository is cloned to the test cache directory.
/// Returns the path to the cloned repo.
fn ensure_repo(name: &str, url: &str, git_ref: &str) -> PathBuf {
    let cache_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(".repos");
    let repo_dir = cache_dir.join(format!("{}-{}", name, git_ref));

    if repo_dir.join(".git").exists() {
        return repo_dir;
    }

    std::fs::create_dir_all(&cache_dir).expect("Failed to create .repos cache dir");

    let status = Command::new("git")
        .args(["clone", "--depth", "1", "--branch", git_ref, url])
        .arg(&repo_dir)
        .status()
        .expect("Failed to run git clone");

    assert!(
        status.success(),
        "git clone failed for {} @ {}",
        url,
        git_ref
    );

    repo_dir
}

/// Compute SHA-256 hex digest of a file.
fn sha256_file(path: &Path) -> String {
    let bytes =
        std::fs::read(path).unwrap_or_else(|e| panic!("Failed to read {}: {}", path.display(), e));
    let digest = Sha256::digest(&bytes);
    format!("{:x}", digest)
}

/// Clean the target directory inside a project to ensure a fresh build.
fn clean_target(project_dir: &Path) {
    let target = project_dir.join("target");
    if target.exists() {
        let _ = std::fs::remove_dir_all(&target);
    }
}

// ---------------------------------------------------------------------------
// Fixture tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn fixture_minimal_binary() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal-bin");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    // Verify the binary was produced
    let binary = fixture_dir.join("target/debug/minimal-bin");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    // Verify it runs
    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello from minimal-bin"),
        "Unexpected output: {}",
        stdout
    );
}

#[test]
#[ignore]
fn fixture_workspace_binaries() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace-bins");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    // Verify both binaries were produced
    let bin_a = fixture_dir.join("target/debug/bin-a");
    let bin_b = fixture_dir.join("target/debug/bin-b");
    assert!(bin_a.exists(), "bin-a not found at {}", bin_a.display());
    assert!(bin_b.exists(), "bin-b not found at {}", bin_b.display());

    // Verify both run correctly
    let output_a = Command::new(&bin_a).output().expect("Failed to run bin-a");
    assert!(output_a.status.success());
    let stdout_a = String::from_utf8_lossy(&output_a.stdout);
    assert!(
        stdout_a.contains("hello, world!"),
        "bin-a unexpected output: {}",
        stdout_a
    );

    let output_b = Command::new(&bin_b).output().expect("Failed to run bin-b");
    assert!(output_b.status.success());
    let stdout_b = String::from_utf8_lossy(&output_b.stdout);
    assert!(
        stdout_b.contains("hello, cargo-schnee!"),
        "bin-b unexpected output: {}",
        stdout_b
    );
}

// Workspace with proc-macro member, build script, and external dep
#[test]
#[ignore]
fn fixture_workspace_advanced() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace-advanced");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    let binary = fixture_dir.join("target/debug/app");
    assert!(
        binary.exists(),
        "app binary not found at {}",
        binary.display()
    );

    let output = Command::new(&binary).output().expect("Failed to run app");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Verify proc-macro derived name() works
    assert!(stdout.contains("name=App"), "proc-macro failed: {}", stdout);
    // Verify build script env var propagated
    assert!(
        stdout.contains("stamp=workspace-advanced-test"),
        "build script env missing: {}",
        stdout,
    );
    // Verify external crate (itoa) works
    assert!(
        stdout.contains("answer=42"),
        "external dep failed: {}",
        stdout
    );
}

// Warm workspace rebuild produces identical binaries
#[test]
#[ignore]
fn fixture_workspace_warm_rebuild() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace-bins");
    let manifest = fixture_dir.join("Cargo.toml");
    let bin_a = fixture_dir.join("target/debug/bin-a");
    let bin_b = fixture_dir.join("target/debug/bin-b");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);
    let hash_a1 = sha256_file(&bin_a);
    let hash_b1 = sha256_file(&bin_b);

    // Second build (warm) — should produce identical binaries
    run_schnee_build(&manifest);
    let hash_a2 = sha256_file(&bin_a);
    let hash_b2 = sha256_file(&bin_b);

    assert_eq!(hash_a1, hash_a2, "warm rebuild changed bin-a");
    assert_eq!(hash_b1, hash_b2, "warm rebuild changed bin-b");
}

// B6: warm build produces identical binary
#[test]
#[ignore]
fn fixture_minimal_binary_warm_rebuild() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal-bin");
    let manifest = fixture_dir.join("Cargo.toml");
    let binary = fixture_dir.join("target/debug/minimal-bin");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);
    let hash1 = sha256_file(&binary);

    // Second build (warm) — should produce identical binary
    run_schnee_build(&manifest);
    let hash2 = sha256_file(&binary);

    assert_eq!(hash1, hash2, "warm build produced different binary");
}

// B7: verify --verify-drv-paths flag
#[test]
#[ignore]
fn fixture_verify_drv_paths() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal-bin");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);

    let output = Command::new(cargo_schnee_bin())
        .arg("schnee")
        .arg("build")
        .arg("--verify-drv-paths")
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .expect("Failed to execute cargo-schnee");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "cargo-schnee build --verify-drv-paths failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr,
    );
}

// B8: concurrent builds don't interfere
#[test]
#[ignore]
fn concurrent_builds() {
    let minimal_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal-bin");
    let workspace_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace-bins");

    clean_target(&minimal_dir);
    clean_target(&workspace_dir);

    let minimal_manifest = minimal_dir.join("Cargo.toml");
    let workspace_manifest = workspace_dir.join("Cargo.toml");
    let bin = cargo_schnee_bin();

    let bin1 = bin.clone();
    let m1 = minimal_manifest.clone();
    let t1 = std::thread::spawn(move || {
        let output = Command::new(bin1)
            .arg("schnee")
            .arg("build")
            .arg("--manifest-path")
            .arg(&m1)
            .output()
            .expect("Failed to execute cargo-schnee");
        assert!(
            output.status.success(),
            "concurrent minimal build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    let bin2 = bin;
    let m2 = workspace_manifest.clone();
    let t2 = std::thread::spawn(move || {
        let output = Command::new(bin2)
            .arg("schnee")
            .arg("build")
            .arg("--manifest-path")
            .arg(&m2)
            .output()
            .expect("Failed to execute cargo-schnee");
        assert!(
            output.status.success(),
            "concurrent workspace build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    });

    t1.join().expect("minimal build thread panicked");
    t2.join().expect("workspace build thread panicked");
}

// ---------------------------------------------------------------------------
// GitHub project tests
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn github_hyperfine() {
    let repo = ensure_repo(
        "hyperfine",
        "https://github.com/sharkdp/hyperfine.git",
        "v1.19.0",
    );
    let manifest = repo.join("Cargo.toml");

    clean_target(&repo);
    run_schnee_build(&manifest);

    let binary = repo.join("target/debug/hyperfine");
    assert!(
        binary.exists(),
        "hyperfine binary not found at {}",
        binary.display()
    );

    let output = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("Failed to run hyperfine");
    assert!(output.status.success(), "hyperfine --version failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hyperfine"),
        "Unexpected version output: {}",
        stdout
    );
}

#[test]
#[ignore]
fn github_just_workspace() {
    let repo = ensure_repo("just", "https://github.com/casey/just.git", "1.40.0");
    let manifest = repo.join("Cargo.toml");

    clean_target(&repo);
    run_schnee_build(&manifest);

    let binary = repo.join("target/debug/just");
    assert!(
        binary.exists(),
        "just binary not found at {}",
        binary.display()
    );

    let output = Command::new(&binary)
        .arg("--version")
        .output()
        .expect("Failed to run just");
    assert!(output.status.success(), "just --version failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("just"),
        "Unexpected version output: {}",
        stdout
    );
}

// Cross-compilation: build minimal-bin for aarch64-unknown-linux-gnu.
// Requires a Rust toolchain with aarch64 target and aarch64-linux-gnu cross-linker.
#[test]
#[ignore]
fn fixture_cross_compile_aarch64() {
    let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal-bin");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);

    let output = Command::new(cargo_schnee_bin())
        .arg("schnee")
        .arg("build")
        .arg("--target")
        .arg("aarch64-unknown-linux-gnu")
        .arg("--manifest-path")
        .arg(&manifest)
        .output()
        .expect("Failed to execute cargo-schnee with --target");

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "cross build failed:\n{}", stderr,);

    // Binary should be in target/{triple}/debug/
    let binary = fixture_dir.join("target/aarch64-unknown-linux-gnu/debug/minimal-bin");
    assert!(
        binary.exists(),
        "Cross-compiled binary not found at {}",
        binary.display()
    );

    // Verify it's an aarch64 ELF
    let file_output = Command::new("file").arg(&binary).output().unwrap();
    let file_str = String::from_utf8_lossy(&file_output.stdout);
    assert!(
        file_str.contains("aarch64") || file_str.contains("ARM aarch64"),
        "Binary is not aarch64: {}",
        file_str
    );
}
