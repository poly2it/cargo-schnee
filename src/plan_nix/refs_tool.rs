//! `schnee-refs`, which every unit with a runtime reference contract runs
//! over its output before Nix checks `allowedReferences`.
//!
//! This file is the whole program. cargo-schnee compiles it as a module so
//! its tests run, and `refs_tool_drv` compiles the same text into a binary
//! inside a derivation. It therefore uses `std` alone.
//!
//! The candidates are the store paths visible to the builder, which in a
//! sandbox are the unit's inputs and its own output. A candidate that is
//! not in the allowed set becomes either a rewrite or a violation:
//!
//! - In a file named by `--scrub`, the hash becomes `e` repeated, which no
//!   store path hash contains. lld-link records every input it read in a
//!   PDB, and clang-cl records each object's own path in it, and neither
//!   offers an option to remap them, so this is the only way to keep them
//!   out of a Windows unit's references.
//! - In a file named by `--rewrite`, `<store>/<hash>-<name>` becomes
//!   `/schnee/<name>`, the name rustc already gives remapped paths.
//! - Anywhere else, the reference is reported with the file and the bytes
//!   around it, and the program exits with status 1.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const HASH_LEN: usize = 32;
const VIRTUAL_ROOT: &str = "/schnee/";

/// Nix's base-32 alphabet, which store path hashes are written in.
fn is_hash_char(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'd' | b'f'..=b'n' | b'p'..=b's' | b'v'..=b'z')
}

/// Byte offsets of every candidate hash in `data`, with the hash found.
///
/// The window is checked from its last byte backwards, so a byte outside
/// the alphabet skips the whole window up to it, the same way Nix's own
/// reference scanner moves through a file.
fn find_hashes<'h>(data: &[u8], hashes: &'h HashMap<Vec<u8>, String>) -> Vec<(usize, &'h [u8])> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + HASH_LEN <= data.len() {
        let window = &data[i..i + HASH_LEN];
        match window.iter().rposition(|&b| !is_hash_char(b)) {
            Some(j) => i += j + 1,
            None => {
                if let Some((hash, _)) = hashes.get_key_value(window) {
                    found.push((i, hash.as_slice()));
                    i += HASH_LEN;
                } else {
                    i += 1;
                }
            }
        }
    }
    found
}

/// Printable bytes around `at`, for a report a person can read.
fn context(data: &[u8], at: usize) -> String {
    let printable = |b: &u8| (0x20..0x7f).contains(b);
    let start = data[..at]
        .iter()
        .rev()
        .take(120)
        .take_while(|b| printable(b))
        .count();
    let end = data[at..]
        .iter()
        .take(160)
        .take_while(|b| printable(b))
        .count();
    String::from_utf8_lossy(&data[at - start..at + end]).into_owned()
}

/// Replaces each candidate hash in `data` with `e` repeated.
fn scrub(data: &mut [u8], hashes: &HashMap<Vec<u8>, String>) -> usize {
    let found: Vec<usize> = find_hashes(data, hashes)
        .into_iter()
        .map(|(at, _)| at)
        .collect();
    for &at in &found {
        data[at..at + HASH_LEN].fill(b'e');
    }
    found.len()
}

/// Replaces `<store>/<hash>-` with `/schnee/` for each candidate hash.
fn rewrite(data: &[u8], store: &str, hashes: &HashMap<Vec<u8>, String>) -> Vec<u8> {
    let prefix = format!("{store}/");
    let mut out = Vec::with_capacity(data.len());
    let mut copied = 0;
    for (at, _) in find_hashes(data, hashes) {
        let starts_path = at >= prefix.len() && &data[at - prefix.len()..at] == prefix.as_bytes();
        let has_name = data.get(at + HASH_LEN) == Some(&b'-');
        if !(starts_path && has_name) {
            continue;
        }
        out.extend_from_slice(&data[copied..at - prefix.len()]);
        out.extend_from_slice(VIRTUAL_ROOT.as_bytes());
        copied = at + HASH_LEN + 1;
    }
    out.extend_from_slice(&data[copied..]);
    out
}

/// What `schnee-refs` was asked to do.
#[derive(Debug, Default, PartialEq)]
struct Args {
    out: PathBuf,
    allowed: Vec<String>,
    scrub_suffixes: Vec<String>,
    rewrite_names: Vec<String>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{arg} takes a value"))
        };
        match arg.as_str() {
            "--allowed" => parsed
                .allowed
                .extend(value()?.split_whitespace().map(String::from)),
            "--scrub" => parsed.scrub_suffixes.push(value()?),
            "--rewrite" => parsed.rewrite_names.push(value()?),
            _ if parsed.out.as_os_str().is_empty() => parsed.out = PathBuf::from(arg),
            _ => return Err(format!("unexpected argument {arg}")),
        }
    }
    if parsed.out.as_os_str().is_empty() {
        return Err(
            "usage: schnee-refs <out> [--allowed <paths>] [--scrub <suffix>] [--rewrite <name>]"
                .into(),
        );
    }
    Ok(parsed)
}

/// The hash of a store path's base name, when it has the shape of one.
fn hash_of(base: &str) -> Option<&str> {
    let hash = base.get(..HASH_LEN)?;
    (base.as_bytes().get(HASH_LEN) == Some(&b'-') && hash.bytes().all(is_hash_char)).then_some(hash)
}

/// Hashes of the store entries visible to the builder that `allowed` does
/// not name, each with the entry's full path.
fn disallowed_candidates(
    store: &Path,
    allowed: &[String],
) -> Result<HashMap<Vec<u8>, String>, String> {
    let allowed: Vec<&str> = allowed
        .iter()
        .filter_map(|p| Path::new(p).file_name()?.to_str())
        .filter_map(hash_of)
        .collect();
    let entries =
        std::fs::read_dir(store).map_err(|e| format!("cannot list {}: {e}", store.display()))?;
    let mut candidates = HashMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(hash) = hash_of(name) else { continue };
        if !allowed.contains(&hash) {
            candidates.insert(
                hash.as_bytes().to_vec(),
                format!("{}/{name}", store.display()),
            );
        }
    }
    Ok(candidates)
}

/// Every regular file and symlink below `dir`, sorted so reports are stable.
fn files_below(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Applies `--scrub` and `--rewrite`, then returns the report of every
/// reference left that is not allowed, empty when there is none.
fn check(args: &Args) -> Result<String, String> {
    let out = &args.out;
    let store = out
        .parent()
        .ok_or_else(|| format!("{} is not a store path", out.display()))?;
    let candidates = disallowed_candidates(store, &args.allowed)?;
    let io = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
    let files = if out.is_dir() {
        files_below(out).map_err(|e| io(out, e))?
    } else {
        vec![out.clone()]
    };
    let mut report = String::new();
    for path in files {
        let meta = std::fs::symlink_metadata(&path).map_err(|e| io(&path, e))?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let data = if meta.file_type().is_symlink() {
            std::fs::read_link(&path)
                .map_err(|e| io(&path, e))?
                .into_os_string()
                .into_encoded_bytes()
        } else if args
            .scrub_suffixes
            .iter()
            .any(|s| name.ends_with(s.as_str()))
        {
            let mut data = std::fs::read(&path).map_err(|e| io(&path, e))?;
            if scrub(&mut data, &candidates) > 0 {
                std::fs::write(&path, &data).map_err(|e| io(&path, e))?;
            }
            continue;
        } else if args.rewrite_names.iter().any(|n| n == name) {
            let data = std::fs::read(&path).map_err(|e| io(&path, e))?;
            let rewritten = rewrite(&data, &store.to_string_lossy(), &candidates);
            if rewritten != data {
                std::fs::write(&path, &rewritten).map_err(|e| io(&path, e))?;
            }
            rewritten
        } else {
            std::fs::read(&path).map_err(|e| io(&path, e))?
        };
        let mut by_path: Vec<(&str, Vec<usize>)> = Vec::new();
        for (at, hash) in find_hashes(&data, &candidates) {
            let target = candidates[hash].as_str();
            match by_path.iter_mut().find(|(p, _)| *p == target) {
                Some((_, offsets)) => offsets.push(at),
                None => by_path.push((target, vec![at])),
            }
        }
        let shown = path.strip_prefix(out).unwrap_or(&path).display();
        for (target, offsets) in by_path {
            let places = match offsets.len() {
                1 => "1 place".to_string(),
                n => format!("{n} places"),
            };
            report.push_str(&format!(
                "cargo-schnee: {shown} refers to {target} in {places}, which is not a runtime reference of this unit:\n"
            ));
            for &at in offsets.iter().take(3) {
                report.push_str(&format!("    at byte {at}: {}\n", context(&data, at)));
            }
        }
    }
    Ok(report)
}

/// The program's entry point, returning its exit status.
#[allow(dead_code)]
pub fn run(args: &[String]) -> i32 {
    let outcome = parse_args(args).and_then(|args| check(&args));
    match outcome {
        Ok(report) if report.is_empty() => 0,
        Ok(report) => {
            let _ = std::io::stderr().write_all(report.as_bytes());
            1
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "schnee-refs: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBC: &str = "pqyiidz5mi73wv5f91d48s9plcqyj5bz";
    const SRC: &str = "ssxwv9a3kdpm99ah9zaww7mbqhxbvyf5";

    fn candidates(hashes: &[&str]) -> HashMap<Vec<u8>, String> {
        hashes
            .iter()
            .map(|h| (h.as_bytes().to_vec(), format!("/nix/store/{h}-x")))
            .collect()
    }

    #[test]
    fn finds_hashes_between_binary_bytes() {
        let data = format!("\0\x01/nix/store/{SRC}-src/main.rs\0{LIBC}");
        let hashes = candidates(&[SRC, LIBC]);
        let found = find_hashes(data.as_bytes(), &hashes);
        let offsets: Vec<usize> = found.iter().map(|(at, _)| *at).collect();
        assert_eq!(offsets, vec![13, 13 + 32 + 13]);
    }

    #[test]
    fn ignores_hashes_that_are_not_candidates() {
        let data = format!("/nix/store/{LIBC}-glibc/lib/ld-linux.so");
        assert!(find_hashes(data.as_bytes(), &candidates(&[SRC])).is_empty());
    }

    #[test]
    fn scrub_keeps_the_length_and_the_name() {
        let mut data = format!("C:\\x /nix/store/{SRC}-win-sdk/libucrt.lib").into_bytes();
        let len = data.len();
        assert_eq!(scrub(&mut data, &candidates(&[SRC])), 1);
        assert_eq!(data.len(), len);
        let text = String::from_utf8(data).unwrap();
        assert!(text.contains(&format!(
            "/nix/store/{}-win-sdk/libucrt.lib",
            "e".repeat(32)
        )));
    }

    #[test]
    fn rewrite_drops_the_store_directory_and_the_hash() {
        let data = format!("error at /nix/store/{SRC}-src/main.rs:3 and bare {SRC}");
        let rewritten = rewrite(data.as_bytes(), "/nix/store", &candidates(&[SRC]));
        assert_eq!(
            String::from_utf8(rewritten).unwrap(),
            format!("error at /schnee/src/main.rs:3 and bare {SRC}")
        );
    }

    #[test]
    fn parses_space_separated_allowed_paths() {
        let args: Vec<String> = ["/nix/store/o", "--allowed", "/a /b", "--scrub", ".pdb"]
            .map(String::from)
            .to_vec();
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.out, PathBuf::from("/nix/store/o"));
        assert_eq!(parsed.allowed, vec!["/a", "/b"]);
        assert_eq!(parsed.scrub_suffixes, vec![".pdb"]);
    }

    #[test]
    fn reports_and_rewrites_inside_a_store_directory() {
        let store = std::env::temp_dir().join(format!("schnee-refs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let src = store.join(format!("{SRC}-src"));
        let libc = store.join(format!("{LIBC}-glibc"));
        let out = store.join(format!("{}-unit", "0".repeat(32)));
        for dir in [&src, &libc, &out] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let store_str = store.to_string_lossy();
        std::fs::write(
            out.join("bin"),
            format!("\0{store_str}/{LIBC}-glibc/lib\0{store_str}/{SRC}-src/main.rs\0"),
        )
        .unwrap();
        std::fs::write(
            out.join("diagnostics"),
            format!("{store_str}/{SRC}-src/main.rs"),
        )
        .unwrap();
        let args = Args {
            out: out.clone(),
            allowed: vec![libc.to_string_lossy().into_owned()],
            rewrite_names: vec!["diagnostics".into()],
            ..Args::default()
        };
        let report = check(&args).unwrap();
        assert!(report.contains("bin refers to"), "{report}");
        assert!(report.contains(&format!("{SRC}-src/main.rs")), "{report}");
        assert!(!report.contains("diagnostics"), "{report}");
        assert!(!report.contains("-glibc"), "{report}");
        assert_eq!(
            std::fs::read_to_string(out.join("diagnostics")).unwrap(),
            "/schnee/src/main.rs"
        );
        std::fs::remove_dir_all(&store).unwrap();
    }
}
