//! Integration tests for cargo-schnee.
//!
//! These tests require a running Nix daemon with ca-derivations enabled.
//! Run with: `cargo test -- --ignored`

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex, MutexGuard};

/// Guards to serialize tests that share a fixture directory.
/// Without this, parallel test runs race on clean_target / build / read.
static MINIMAL_BIN_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static WORKSPACE_BINS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn lock(m: &'static LazyLock<Mutex<()>>) -> MutexGuard<'static, ()> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Get the path to the cargo-schnee binary built by cargo test.
fn cargo_schnee_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-schnee"))
}

/// Run `cargo-schnee build` on the given manifest path and assert success.
/// Returns (stdout, stderr) on success.
fn run_schnee_build(manifest_path: &Path) -> (String, String) {
    run_schnee_cmd("build", manifest_path, &[])
}

/// Run `cargo-schnee test` on the given manifest path and assert success.
/// Returns (stdout, stderr) on success.
fn run_schnee_test(manifest_path: &Path) -> (String, String) {
    run_schnee_cmd("test", manifest_path, &[])
}

/// Run a cargo-schnee subcommand on the given manifest path and assert success.
fn run_schnee_cmd(subcmd: &str, manifest_path: &Path, extra_args: &[&str]) -> (String, String) {
    let output = Command::new(cargo_schnee_bin())
        .arg("schnee")
        .arg(subcmd)
        .arg("--manifest-path")
        .arg(manifest_path)
        .args(extra_args)
        .output()
        .expect("Failed to execute cargo-schnee");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "cargo-schnee {} failed for {}:\nstdout:\n{}\nstderr:\n{}",
        subcmd,
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
        .join("repos");
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

    // Ensure the cloned repo has a [workspace] marker so cargo doesn't walk up
    // to the cargo-schnee project root when running `cargo vendor` etc.
    let cargo_toml = repo_dir.join("Cargo.toml");
    if cargo_toml.exists() {
        let contents = std::fs::read_to_string(&cargo_toml).unwrap();
        if !contents.contains("[workspace]") {
            std::fs::write(&cargo_toml, format!("[workspace]\n\n{contents}")).unwrap();
        }
    }

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
    let _guard = lock(&MINIMAL_BIN_LOCK);
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
    let _guard = lock(&WORKSPACE_BINS_LOCK);
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

// Workspace using members = ["*"] glob pattern
#[test]
#[ignore]
fn fixture_workspace_glob_members() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace-glob");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    let alpha = fixture_dir.join("target/debug/alpha");
    let beta = fixture_dir.join("target/debug/beta");
    assert!(
        alpha.exists(),
        "alpha binary not found at {}",
        alpha.display()
    );
    assert!(beta.exists(), "beta binary not found at {}", beta.display());

    let output = Command::new(&alpha).output().expect("Failed to run alpha");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("hello from alpha"),
        "Unexpected alpha output"
    );

    let output = Command::new(&beta).output().expect("Failed to run beta");
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("hello from beta"),
        "Unexpected beta output"
    );
}

// Warm workspace rebuild produces identical binaries
#[test]
#[ignore]
fn fixture_workspace_warm_rebuild() {
    let _guard = lock(&WORKSPACE_BINS_LOCK);
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
    let _guard = lock(&MINIMAL_BIN_LOCK);
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
    let _guard = lock(&MINIMAL_BIN_LOCK);
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

// Verify that the pre-flight system library check emits a warning when a
// -sys crate's `links` value isn't discoverable via pkg-config.
#[test]
#[ignore]
fn fixture_sys_lib_check_warning() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sys-lib-check");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    let (_stdout, stderr) = run_schnee_build(&manifest);

    // The fake-sys crate declares `links = "nonexistent_test_lib_xyz"` which
    // cannot exist in any pkg-config database.  cargo-schnee should warn.
    assert!(
        stderr.contains("nonexistent_test_lib_xyz"),
        "Expected warning about missing system library 'nonexistent_test_lib_xyz' in stderr:\n{}",
        stderr,
    );
    assert!(
        stderr.contains("not found via pkg-config"),
        "Expected actionable pkg-config hint in stderr:\n{}",
        stderr,
    );
    assert!(
        stderr.contains("buildInputs"),
        "Expected buildInputs suggestion in stderr:\n{}",
        stderr,
    );

    // The build should still succeed (build script is a no-op).
    let binary = fixture_dir.join("target/debug/sys-lib-check-app");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("lib=fake-sys"),
        "Unexpected output: {}",
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

/// External path dependency: project depends on a sibling crate outside its
/// directory. cargo-schnee should auto-detect the external dep, copy it into
/// the store tree, and rewrite Cargo.toml paths.
#[test]
#[ignore]
fn fixture_external_path_dep() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/external-path-dep");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    // Verify the binary was produced
    let binary = fixture_dir.join("target/debug/external-path-dep");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    // Verify it runs and uses the external lib
    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello from external-dep-lib"),
        "Unexpected output: {}",
        stdout
    );
}

/// Workspace with `members = ["*"]` and an external path dependency.
/// This catches regressions where external deps copied into the store tree
/// are matched by the workspace glob but not excluded, causing cargo to
/// fail loading them as workspace members.
#[test]
#[ignore]
fn fixture_workspace_glob_external() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace-glob-external");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    // Verify the binary was produced
    let binary = fixture_dir.join("target/debug/app");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    // Verify it runs and uses the external lib
    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello from external-dep-lib"),
        "Unexpected output: {}",
        stdout
    );
}

/// Same as fixture_workspace_glob_external but invoked from the member's
/// manifest instead of the workspace root. Catches the bug where Cargo.lock
/// isn't found because it lives at the workspace root, not in the member dir.
#[test]
#[ignore]
fn fixture_workspace_glob_external_from_member() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace-glob-external");
    let manifest = fixture_dir.join("app/Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    let binary = fixture_dir.join("target/debug/app");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello from external-dep-lib"),
        "Unexpected output: {}",
        stdout
    );
}

/// Build script that writes to $HOME. In a Nix sandbox $HOME is normally
/// unwritable (/homeless-shelter). cargo-schnee sets HOME=$TMPDIR so that
/// crates spawning embedded engines or writing temp files under $HOME work.
#[test]
#[ignore]
fn fixture_build_script_home() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build-script-home");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    let binary = fixture_dir.join("target/debug/build-script-home");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("build-script-home ok"),
        "Unexpected output: {}",
        stdout
    );
}

/// Build script that copies directory trees from CARGO_MANIFEST_DIR (Nix store)
/// preserving source permissions. Directories in the Nix store have 555 perms,
/// so the copy creates read-only destination dirs. Writing into them must still
/// succeed — cargo-schnee needs to ensure the build script environment handles this.
#[test]
#[ignore]
fn fixture_build_script_copy() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/build-script-copy");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    let binary = fixture_dir.join("target/debug/build-script-copy");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("build-script-copy ok"),
        "Unexpected output: {}",
        stdout
    );
}

/// Extra includes: gitignored generated files specified via
/// `[package.metadata.schnee] extra-includes` are copied into the store.
/// The test creates the generated file before building (simulating a pre-build
/// step that produces gitignored output).
#[test]
#[ignore]
fn fixture_extra_includes() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extra-includes");
    let manifest = fixture_dir.join("Cargo.toml");

    // Simulate a pre-build step: create the gitignored generated file
    let generated_dir = fixture_dir.join("src/generated");
    std::fs::create_dir_all(&generated_dir).expect("create generated dir");
    std::fs::write(generated_dir.join("data.html"), "<p>generated</p>\n")
        .expect("write generated file");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    let binary = fixture_dir.join("target/debug/extra-includes");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("<p>generated</p>"),
        "Unexpected output: {}",
        stdout
    );
}

/// passthruEnv from Cargo.toml metadata: env var names declared in
/// `[workspace.metadata.schnee] passthruEnv` are forwarded to ALL build script
/// derivations, including transitive dependencies.
#[test]
#[ignore]
fn fixture_passthru_env_manifest() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/passthru-env-manifest");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    let output = Command::new(cargo_schnee_bin())
        .arg("schnee")
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .env("MY_CUSTOM_FLAG", "hello-from-env")
        .output()
        .expect("Failed to execute cargo-schnee");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "cargo-schnee build failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr,
    );

    let binary = fixture_dir.join("target/debug/my-app");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    let run_output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(run_output.status.success());
    let run_stdout = String::from_utf8_lossy(&run_output.stdout);
    assert!(
        run_stdout.contains("flag=hello-from-env"),
        "Unexpected output: {}",
        run_stdout
    );
}

/// Extra includes with _parent: a workspace whose extra-includes glob matches
/// files outside the project directory. A build script in a member crate reads
/// those files via relative paths from CARGO_MANIFEST_DIR. The files are mapped
/// to _parent/ in the source store and must be accessible from the build script
/// workdir.
#[test]
#[ignore]
fn fixture_extra_includes_parent() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extra-includes-parent");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_build(&manifest);

    let binary = fixture_dir.join("target/debug/my-crate");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello from parent data"),
        "Unexpected output: {}",
        stdout
    );
}

/// External path dep pointing at a sub-crate inside another workspace.
/// The sub-crate inherits `edition.workspace = true` from its parent workspace
/// root. cargo-schnee must copy the entire external workspace (not just the
/// sub-crate) so that workspace inheritance still resolves.
#[test]
#[ignore]
fn fixture_external_ws_subcrate() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/external-ws-subcrate");
    let manifest = fixture_dir.join("project/Cargo.toml");

    clean_target(&fixture_dir.join("project"));
    run_schnee_build(&manifest);

    let binary = fixture_dir.join("project/target/debug/ws-subcrate-project");
    assert!(binary.exists(), "Binary not found at {}", binary.display());

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run built binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello from sub-crate"),
        "Unexpected output: {}",
        stdout
    );
}

// Cross-compilation: proc-macro dependency's build script must see TARGET == HOST.
// host-probe is only reachable through the proc-macro, so it's always compiled
// for the host. Its build script asserts TARGET == HOST.
#[test]
#[ignore]
fn fixture_cross_build_script_host_target() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cross-bs-host-target");
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

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "cross build with host build script failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr,
    );

    // Binary is cross-compiled for aarch64 — verify it's the right arch
    let binary = fixture_dir.join("target/aarch64-unknown-linux-gnu/debug/app");
    assert!(
        binary.exists(),
        "Cross-compiled binary not found at {}",
        binary.display()
    );

    let file_output = Command::new("file").arg(&binary).output().unwrap();
    let file_str = String::from_utf8_lossy(&file_output.stdout);
    assert!(
        file_str.contains("aarch64") || file_str.contains("ARM aarch64"),
        "Binary is not aarch64: {}",
        file_str
    );
}

// Cross-compilation: build script emits cargo:rustc-link-search pointing to
// CARGO_MANIFEST_DIR/lib. The path must survive from the build-script sandbox
// to the linking derivation (rewritten from /build/workdir/… to store path).
#[test]
#[ignore]
fn fixture_cross_build_script_link_search() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cross-bs-link-search");
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

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "cross build with link-search failed:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        stderr,
    );

    let binary =
        fixture_dir.join("target/aarch64-unknown-linux-gnu/debug/cross-bs-link-search-app");
    assert!(
        binary.exists(),
        "Cross-compiled binary not found at {}",
        binary.display()
    );
}

// ---------------------------------------------------------------------------
// Example tests
// ---------------------------------------------------------------------------

// Simple single-crate example with serde dependency
#[test]
#[ignore]
fn example_simple() {
    let example_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/simple");
    let manifest = example_dir.join("Cargo.toml");

    clean_target(&example_dir);
    run_schnee_build(&manifest);

    let binary = example_dir.join("target/debug/test-project");
    assert!(
        binary.exists(),
        "test-project binary not found at {}",
        binary.display()
    );

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run test-project");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"name\": \"test\"") && stdout.contains("430"),
        "Unexpected simple example output: {}",
        stdout
    );
}

// Cross-compilation example (native build only — verifies the crate compiles)
#[test]
#[ignore]
fn example_cross() {
    let example_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/cross");
    let manifest = example_dir.join("Cargo.toml");

    clean_target(&example_dir);
    run_schnee_build(&manifest);

    let binary = example_dir.join("target/debug/cross-example");
    assert!(
        binary.exists(),
        "cross-example binary not found at {}",
        binary.display()
    );

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run cross-example");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Hello from cargo-schnee cross-compilation!"),
        "Unexpected cross example output: {}",
        stdout
    );
}

// build-package example: workspace with two binary crates and serde deps
#[test]
#[ignore]
fn example_build_package() {
    let example_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/build-package");
    let manifest = example_dir.join("Cargo.toml");

    clean_target(&example_dir);
    run_schnee_build(&manifest);

    // Verify both workspace binaries were produced
    let greeter = example_dir.join("target/debug/greeter");
    let formatter = example_dir.join("target/debug/formatter");
    assert!(
        greeter.exists(),
        "greeter binary not found at {}",
        greeter.display()
    );
    assert!(
        formatter.exists(),
        "formatter binary not found at {}",
        formatter.display()
    );

    // greeter outputs JSON with a greeting message
    let output = Command::new(&greeter)
        .output()
        .expect("Failed to run greeter");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("hello from cargo-schnee buildPackage"),
        "Unexpected greeter output: {}",
        stdout
    );

    // formatter reads JSON from stdin and formats it
    let output = Command::new(&formatter)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"{\"message\": \"test\"}")
                .unwrap();
            child.wait_with_output()
        })
        .expect("Failed to run formatter");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[formatted] test"),
        "Unexpected formatter output: {}",
        stdout
    );
}

// build-package-cross example: lib.buildPackage with hostPkgs (native build only)
#[test]
#[ignore]
fn example_build_package_cross() {
    let example_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/build-package-cross");
    let manifest = example_dir.join("Cargo.toml");

    clean_target(&example_dir);
    run_schnee_build(&manifest);

    let binary = example_dir.join("target/debug/build-package-cross-example");
    assert!(
        binary.exists(),
        "build-package-cross-example binary not found at {}",
        binary.display()
    );

    let output = Command::new(&binary)
        .output()
        .expect("Failed to run build-package-cross-example");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Hello from cargo-schnee cross-compilation!"),
        "Unexpected build-package-cross example output: {}",
        stdout
    );
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

/// Test that `cargo schnee test` sets CARGO_MANIFEST_DIR to a writable project
/// path — both the compile-time `env!()` value and the runtime env var.
/// Regression test for: test binaries couldn't write to CARGO_MANIFEST_DIR
/// because it pointed to a read-only nix store path.
#[test]
#[ignore]
fn fixture_test_manifest_dir_writable() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test-manifest-dir-writable");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_test(&manifest);
}

/// Building from a workspace member's manifest should scope the build to
/// that member only, matching standard `cargo` behaviour.
/// Regression test for: cargo test from a subcrate builds the entire workspace.
#[test]
#[ignore]
fn fixture_workspace_member_scoping() {
    let _g = lock(&WORKSPACE_BINS_LOCK);
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace-bins");

    clean_target(&fixture_dir);

    // Build from bin-a's manifest (not the workspace root)
    let manifest = fixture_dir.join("bin-a/Cargo.toml");
    run_schnee_build(&manifest);

    // bin-a should be built
    let bin_a = fixture_dir.join("target/debug/bin-a");
    assert!(
        bin_a.exists(),
        "bin-a should be built when scoped to bin-a: {}",
        bin_a.display()
    );

    // bin-b should NOT be built (workspace scoping)
    let bin_b = fixture_dir.join("target/debug/bin-b");
    assert!(
        !bin_b.exists(),
        "bin-b should NOT be built when scoped to bin-a: {}",
        bin_b.display()
    );
}

/// Crate with both lib and bin targets plus integration tests.
/// Regression test for: rlib artifact missing lib prefix and .rlib extension
/// when the crate also has a bin target, causing integration tests to fail with
/// "extern location for X is of an unknown type".
#[test]
#[ignore]
fn fixture_lib_bin_integration_test() {
    let fixture_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lib-bin-integration");
    let manifest = fixture_dir.join("Cargo.toml");

    clean_target(&fixture_dir);
    run_schnee_test(&manifest);
}
