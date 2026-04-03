use std::collections::HashMap;
use std::fs;
use std::path::Path;

struct Build {
    name: String,
    start: f64,
    stop: f64,
}

impl Build {
    fn time(&self) -> f64 {
        self.stop - self.start
    }
}

fn parse_profile(path: &Path) -> Vec<Build> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut pending: HashMap<u64, (String, f64)> = HashMap::new();
    let mut builds: Vec<Build> = Vec::new();

    for line in content.lines() {
        let Some(rest) = line.strip_prefix('[') else {
            continue;
        };
        let Some(bracket_end) = rest.find(']') else {
            continue;
        };
        let ts: f64 = match rest[..bracket_end].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(json_str) = rest[bracket_end + 1..].strip_prefix(" @nix ") else {
            continue;
        };
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(json_str) else {
            continue;
        };

        let action = entry.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let id = entry.get("id").and_then(|v| v.as_u64()).unwrap_or(0);

        match action {
            "start" if entry.get("type").and_then(|v| v.as_u64()) == Some(105) => {
                let drv = entry
                    .get("fields")
                    .and_then(|f| f.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                pending.insert(id, (extract_drv_name(drv), ts));
            }
            "stop" => {
                if let Some((name, start)) = pending.remove(&id) {
                    builds.push(Build {
                        name,
                        start,
                        stop: ts,
                    });
                }
            }
            _ => {}
        }
    }

    builds.sort_by(|a, b| b.time().partial_cmp(&a.time()).unwrap());
    builds
}

fn extract_drv_name(drv: &str) -> String {
    let basename = drv
        .strip_prefix("/nix/store/")
        .unwrap_or(drv)
        .strip_suffix(".drv")
        .unwrap_or(drv);
    // Skip 32-char hash + dash.
    if basename.len() > 33 && basename.as_bytes().get(32) == Some(&b'-') {
        basename[33..].to_string()
    } else {
        basename.to_string()
    }
}

pub fn run(dir: &str) {
    let sep = "=".repeat(70);
    let mut entries: Vec<_> = match fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            eprintln!("Cannot read directory {dir}: {e}");
            std::process::exit(1);
        }
    };
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }

        let builds = parse_profile(&path);
        if builds.is_empty() {
            continue;
        }

        let label = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .replace("profile-", "");

        let total: f64 = builds.iter().map(|b| b.time()).sum();
        let first_start = builds.iter().map(|b| b.start).reduce(f64::min).unwrap();
        let last_stop = builds.iter().map(|b| b.stop).reduce(f64::max).unwrap();
        let wall_clock = last_stop - first_start;

        println!();
        println!("{sep}");
        println!("  {label}  ({} derivations)", builds.len());
        println!("  Wall clock: {wall_clock:.1}s   Sum of build times: {total:.1}s");
        if builds.len() > 1 {
            println!("  Parallelism: {:.1}x", total / wall_clock);
        }
        println!("{sep}");
        println!("  {:>8}  {:>6}  Name", "Time", "Cum%");
        println!("  {:>8}  {:>6}  ----", "----", "----");

        let show = builds.len().min(30);
        let mut cum = 0.0;
        for b in &builds[..show] {
            cum += b.time();
            let pct = if total > 0.0 { cum / total * 100.0 } else { 0.0 };
            println!("  {:>7.1}s  {:>5.1}%  {}", b.time(), pct, b.name);
        }
        if builds.len() > show {
            let remaining: f64 = builds[show..].iter().map(|b| b.time()).sum();
            println!(
                "  {:>7.1}s         ... and {} more derivations",
                remaining,
                builds.len() - show
            );
        }
    }
}
