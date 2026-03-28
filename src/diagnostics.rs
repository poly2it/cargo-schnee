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

/// Remap Nix store source paths to local project paths in a string.
fn remap_paths(text: &str, src_store_prefix: &str, project_dir_prefix: &str) -> String {
    text.replace(src_store_prefix, project_dir_prefix)
}

/// Process a line from nix-store --realise stderr.
///
/// Only rustc JSON diagnostics (with a `rendered` field) are remapped and rendered.
/// All other lines (build script output, non-JSON nix messages, etc.) are silently dropped.
/// Returns `true` if the line was a JSON diagnostic (rendered or suppressed summary).
pub fn emit_line(
    shell: &mut Shell,
    line: &str,
    src_store_prefix: &str,
    project_dir_prefix: &str,
) -> bool {
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
    let remapped = remap_paths(&rendered, src_store_prefix, project_dir_prefix);
    // print_ansi_stderr handles ANSI → terminal color translation (or stripping if piped)
    let _ = shell.print_ansi_stderr(remapped.as_bytes());
    true
}

/// Replay diagnostics from a file saved in a derivation output.
/// Silently returns if the file doesn't exist or is empty.
pub fn replay_diagnostics_from_file(
    shell: &mut Shell,
    diagnostics_path: &std::path::Path,
    src_store_prefix: &str,
    project_dir_prefix: &str,
) {
    let file = match std::fs::File::open(diagnostics_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let reader = std::io::BufReader::new(file);
    use std::io::BufRead;
    for line in reader.lines().map_while(Result::ok) {
        if !line.is_empty() {
            emit_line(shell, &line, src_store_prefix, project_dir_prefix);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_returns_true_for_diagnostic() {
        let json = r#"{"rendered":"warning: unused variable\n","message":"unused variable","level":"warning","code":null,"spans":[],"children":[]}"#;
        let mut shell = Shell::new();
        assert!(emit_line(
            &mut shell,
            json,
            "/nix/store/src/",
            "/home/user/"
        ));
    }

    #[test]
    fn emit_returns_true_for_artifact_json() {
        let json = r#"{"artifact":"/nix/store/foo","emit":"link"}"#;
        let mut shell = Shell::new();
        assert!(emit_line(
            &mut shell,
            json,
            "/nix/store/src/",
            "/home/user/"
        ));
    }

    #[test]
    fn emit_returns_false_for_non_json() {
        let mut shell = Shell::new();
        assert!(!emit_line(
            &mut shell,
            "building '/nix/store/foo.drv'...",
            "/nix/store/src/",
            "/home/user/"
        ));
        assert!(!emit_line(&mut shell, "", "/nix/store/src/", "/home/user/"));
    }

    #[test]
    fn emit_returns_true_for_summary_messages() {
        let mut shell = Shell::new();

        let json = r#"{"rendered":"aborting due to 3 previous errors\n"}"#;
        assert!(emit_line(
            &mut shell,
            json,
            "/nix/store/src/",
            "/home/user/"
        ));

        let json = r#"{"rendered":"warning: 2 warnings emitted\n"}"#;
        assert!(emit_line(
            &mut shell,
            json,
            "/nix/store/src/",
            "/home/user/"
        ));

        let json = r#"{"rendered":"warning: 1 warning emitted\n"}"#;
        assert!(emit_line(
            &mut shell,
            json,
            "/nix/store/src/",
            "/home/user/"
        ));
    }

    #[test]
    fn remap_nix_paths() {
        let text = "/nix/store/abc123-project-src/src/main.rs:5:1 warning: unused";
        let remapped = remap_paths(
            text,
            "/nix/store/abc123-project-src/",
            "/home/user/project/",
        );
        assert_eq!(
            remapped,
            "/home/user/project/src/main.rs:5:1 warning: unused"
        );
    }

    #[test]
    fn remap_no_match() {
        let text = "error: some other message";
        let remapped = remap_paths(text, "/nix/store/xyz/", "/home/user/project/");
        assert_eq!(remapped, text);
    }
}
