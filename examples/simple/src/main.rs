use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    name: String,
    value: i64,
}

fn main() {
    let config = Config {
        name: "test".to_string(),
        value: 430,
    };
    let json = serde_json::to_string_pretty(&config).unwrap();
    println!("{}", json);
}
