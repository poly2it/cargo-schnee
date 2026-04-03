use crate::generate_markdown;
use std::fs;

const BEGIN_MARKER: &str = "<!-- BEGIN BENCHMARK -->";
const END_MARKER: &str = "<!-- END BENCHMARK -->";

/// Demote markdown headings by one level (# -> ##, ## -> ###, etc.).
fn demote_headings(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for line in md.lines() {
        if line.starts_with('#') {
            out.push('#');
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub fn run(results_path: &str, readme_path: &str) {
    let readme = match fs::read_to_string(readme_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Cannot read {readme_path}: {e}");
            std::process::exit(1);
        }
    };

    let Some(begin) = readme.find(BEGIN_MARKER) else {
        eprintln!("{readme_path}: missing {BEGIN_MARKER}");
        std::process::exit(1);
    };
    let Some(end) = readme.find(END_MARKER) else {
        eprintln!("{readme_path}: missing {END_MARKER}");
        std::process::exit(1);
    };

    let md = generate_markdown::generate(results_path);
    let inlined = demote_headings(&md);

    let mut out = String::with_capacity(readme.len());
    out.push_str(&readme[..begin + BEGIN_MARKER.len()]);
    out.push('\n');
    out.push_str(&inlined);
    out.push_str(&readme[end..]);

    fs::write(readme_path, &out).unwrap_or_else(|e| {
        eprintln!("Cannot write {readme_path}: {e}");
        std::process::exit(1);
    });
}
