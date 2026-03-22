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

/// Try to parse a line as a rustc JSON diagnostic.
/// Returns the `rendered` field if present, None for non-diagnostic JSON (artifacts) or non-JSON.
fn try_parse_diagnostic(line: &str) -> Option<String> {
    let msg: CompilerMessage = serde_json::from_str(line).ok()?;
    let rendered = msg.rendered?;
    // Skip summary messages that cargo normally suppresses
    if rendered.contains("aborting due to")
        || rendered.contains("warning emitted")
        || rendered.contains("warnings emitted")
    {
        return None;
    }
    Some(rendered)
}

/// Remap Nix store source paths to local project paths in a string.
fn remap_paths(text: &str, src_store_prefix: &str, project_dir_prefix: &str) -> String {
    text.replace(src_store_prefix, project_dir_prefix)
}

/// Process a line from nix-store --realise stderr.
///
/// If the line is a rustc JSON diagnostic, parse it, remap paths, and render via Shell.
/// Otherwise, remap paths and print as-is.
pub fn emit_line(shell: &mut Shell, line: &str, src_store_prefix: &str, project_dir_prefix: &str) {
    if let Some(rendered) = try_parse_diagnostic(line) {
        let remapped = remap_paths(&rendered, src_store_prefix, project_dir_prefix);
        // print_ansi_stderr handles ANSI → terminal color translation (or stripping if piped)
        let _ = shell.print_ansi_stderr(remapped.as_bytes());
    } else if !line.starts_with('{') {
        // Non-JSON line (nix messages, etc.) — remap paths and print
        let remapped = remap_paths(line, src_store_prefix, project_dir_prefix);
        eprintln!("{}", remapped);
    }
    // JSON lines without a `rendered` field (artifacts, etc.) are silently dropped
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
    fn parse_valid_diagnostic() {
        let json = r#"{"rendered":"warning: unused variable\n","message":"unused variable","level":"warning","code":null,"spans":[],"children":[]}"#;
        let rendered = try_parse_diagnostic(json);
        assert_eq!(rendered, Some("warning: unused variable\n".to_string()));
    }

    #[test]
    fn parse_artifact_json() {
        let json = r#"{"artifact":"/nix/store/foo","emit":"link"}"#;
        assert!(try_parse_diagnostic(json).is_none());
    }

    #[test]
    fn parse_non_json() {
        assert!(try_parse_diagnostic("building '/nix/store/foo.drv'...").is_none());
        assert!(try_parse_diagnostic("").is_none());
    }

    #[test]
    fn skip_summary_messages() {
        let json = r#"{"rendered":"aborting due to 3 previous errors\n"}"#;
        assert!(try_parse_diagnostic(json).is_none());

        let json = r#"{"rendered":"warning: 2 warnings emitted\n"}"#;
        assert!(try_parse_diagnostic(json).is_none());

        let json = r#"{"rendered":"warning: 1 warning emitted\n"}"#;
        assert!(try_parse_diagnostic(json).is_none());
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
