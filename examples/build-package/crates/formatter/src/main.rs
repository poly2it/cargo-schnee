use serde::Deserialize;

#[derive(Deserialize)]
struct Input {
    message: String,
}

fn main() {
    let input: Input = serde_json::from_reader(std::io::stdin()).expect("invalid JSON on stdin");
    println!("[formatted] {}", input.message);
}
