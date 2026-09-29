//! Rustc diagnostic parsing and rendering.
//!
//! Parses JSON diagnostics emitted by `rustc --error-format=json --json=diagnostic-rendered-ansi`,
//! remaps Nix store paths to local project paths, and renders via cargo's `Shell::print_ansi_stderr()`.

use cargo::core::shell::Shell;

/// A rustc compiler message (subset of fields we care about).
#[derive(serde::Deserialize)]
struct CompilerMessage {
    rendered: Option<String>,
}

/// `(store prefix, checkout prefix)` pairs, each ending in `/`, that map the
/// source paths rustc saw in the sandbox back to the user's checkout.
pub type PathRemaps = [(String, String)];

/// Remap Nix store source paths to local project paths in a string.
fn remap_paths(text: &str, remaps: &PathRemaps) -> String {
    remaps
        .iter()
        .fold(text.to_string(), |acc, (store, checkout)| {
            acc.replace(store.as_str(), checkout)
        })
}

/// Process a line from nix-store --realise stderr.
///
/// Only rustc JSON diagnostics (with a `rendered` field) are remapped and rendered.
/// All other lines (build script output, non-JSON nix messages, etc.) are silently dropped.
/// Returns `true` if the line was a JSON diagnostic (rendered or suppressed summary).
pub fn emit_line(shell: &mut Shell, line: &str, remaps: &PathRemaps) -> bool {
    // Check if this is valid JSON with a rendered field at all
    let Ok(msg) = serde_json::from_str::<CompilerMessage>(line) else {
        return false;
    };
    let Some(rendered) = msg.rendered else {
        // Valid JSON but no rendered field (artifact notification, etc.) — still a diagnostic line
        return true;
    };
    // Skip summary messages that cargo normally suppresses
    if rendered.contains("aborting due to")
        || rendered.contains("warning emitted")
        || rendered.contains("warnings emitted")
    {
        return true;
    }
    let remapped = remap_paths(&rendered, remaps);
    // print_ansi_stderr handles ANSI → terminal color translation (or stripping if piped)
    let _ = shell.print_ansi_stderr(remapped.as_bytes());
    true
}

/// Replay diagnostics from a file saved in a derivation output.
/// Silently returns if the file doesn't exist or is empty.
pub fn replay_diagnostics_from_file(
    shell: &mut Shell,
    diagnostics_path: &std::path::Path,
    remaps: &PathRemaps,
) {
    let file = match std::fs::File::open(diagnostics_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = std::io::BufReader::new(file);
    use std::io::BufRead;
    for line in reader.lines().map_while(Result::ok) {
        if !line.is_empty() {
            emit_line(shell, &line, remaps);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remaps() -> Vec<(String, String)> {
        vec![("/nix/store/src/".into(), "/home/user/".into())]
    }

    #[test]
    fn emit_returns_true_for_diagnostic() {
        let json = r#"{"rendered":"warning: unused variable\n","message":"unused variable","level":"warning","code":null,"spans":[],"children":[]}"#;
        let mut shell = Shell::new();
        assert!(emit_line(&mut shell, json, &remaps()));
    }

    #[test]
    fn emit_returns_true_for_artifact_json() {
        let json = r#"{"artifact":"/nix/store/foo","emit":"link"}"#;
        let mut shell = Shell::new();
        assert!(emit_line(&mut shell, json, &remaps()));
    }

    #[test]
    fn emit_returns_false_for_non_json() {
        let mut shell = Shell::new();
        assert!(!emit_line(
            &mut shell,
            "building '/nix/store/foo.drv'...",
            &remaps()
        ));
        assert!(!emit_line(&mut shell, "", &remaps()));
    }

    #[test]
    fn emit_returns_true_for_summary_messages() {
        let mut shell = Shell::new();

        let json = r#"{"rendered":"aborting due to 3 previous errors\n"}"#;
        assert!(emit_line(&mut shell, json, &remaps()));

        let json = r#"{"rendered":"warning: 2 warnings emitted\n"}"#;
        assert!(emit_line(&mut shell, json, &remaps()));

        let json = r#"{"rendered":"warning: 1 warning emitted\n"}"#;
        assert!(emit_line(&mut shell, json, &remaps()));
    }

    #[test]
    fn remap_nix_paths() {
        let text = "/nix/store/abc123-project-src/src/main.rs:5:1 warning: unused";
        let remapped = remap_paths(
            text,
            &[(
                "/nix/store/abc123-project-src/".into(),
                "/home/user/project/".into(),
            )],
        );
        assert_eq!(
            remapped,
            "/home/user/project/src/main.rs:5:1 warning: unused"
        );
    }

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Units run rustc with `--json=diagnostic-rendered-ansi,artifacts`, so a
    /// saved `diagnostics` file interleaves artifact notices with diagnostics.
    /// The replay prints only the rendered diagnostic.
    #[test]
    fn replay_skips_artifact_notices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics");
        std::fs::write(
            &path,
            concat!(
                r#"{"$message_type":"artifact","artifact":"/nix/store/out/lib.d","emit":"dep-info"}"#,
                "\n",
                r#"{"$message_type":"diagnostic","rendered":"warning: unused /nix/store/src/lib.rs\n"}"#,
                "\n",
                r#"{"$message_type":"artifact","artifact":"/nix/store/out/liba.rmeta","emit":"metadata"}"#,
                "\n",
            ),
        )
        .unwrap();
        let out = Captured::default();
        let mut shell = Shell::from_write(Box::new(out.clone()));
        replay_diagnostics_from_file(&mut shell, &path, &remaps());
        let printed = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert_eq!(printed, "warning: unused /home/user/lib.rs\n");
    }

    #[test]
    fn remap_every_sliced_crate() {
        let text = " --> /nix/store/aaa-warn-bin/src/main.rs:2:9\n \
                    --> /nix/store/bbb-warn-lib/src/lib.rs:2:9";
        let remapped = remap_paths(
            text,
            &[
                ("/nix/store/aaa-warn-bin/".into(), "/ws/warn-bin/".into()),
                ("/nix/store/bbb-warn-lib/".into(), "/ws/warn-lib/".into()),
            ],
        );
        assert_eq!(
            remapped,
            " --> /ws/warn-bin/src/main.rs:2:9\n --> /ws/warn-lib/src/lib.rs:2:9"
        );
    }

    #[test]
    fn remap_no_match() {
        let text = "error: some other message";
        let remapped = remap_paths(text, &remaps());
        assert_eq!(remapped, text);
    }
}
