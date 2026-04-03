mod generate_markdown;
mod inline_readme;
mod parse_profiles;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: bench-tools <parse-profiles|generate-markdown|inline-readme> [args...]");
        std::process::exit(1);
    }
    match args[1].as_str() {
        "parse-profiles" => {
            let dir = args.get(2).map(|s| s.as_str()).unwrap_or(".");
            parse_profiles::run(dir);
        }
        "generate-markdown" => {
            let path = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: bench-tools generate-markdown <results.json>");
                std::process::exit(1);
            });
            generate_markdown::run(path);
        }
        "inline-readme" => {
            let results = args.get(2).unwrap_or_else(|| {
                eprintln!("Usage: bench-tools inline-readme <results.json> <README.md>");
                std::process::exit(1);
            });
            let readme = args.get(3).unwrap_or_else(|| {
                eprintln!("Usage: bench-tools inline-readme <results.json> <README.md>");
                std::process::exit(1);
            });
            inline_readme::run(results, readme);
        }
        other => {
            eprintln!("Unknown subcommand: {other}");
            std::process::exit(1);
        }
    }
}
