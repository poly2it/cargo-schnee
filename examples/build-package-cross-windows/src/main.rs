use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct Info {
    arch: &'static str,
    os: &'static str,
    message: String,
}

fn main() {
    let info = Info {
        arch: std::env::consts::ARCH,
        os: std::env::consts::OS,
        message: "Hello from cargo-schnee Windows cross-compilation!".into(),
    };
    println!("{}", serde_json::to_string_pretty(&info).unwrap());
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
