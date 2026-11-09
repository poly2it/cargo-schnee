include!(concat!(env!("OUT_DIR"), "/generated.rs"));

extern "C" {
    fn native_answer() -> i32;
}

fn main() {
    let answer = unsafe { native_answer() };
    println!("{} {} {answer}", file!(), GENERATED_FILE);
}
