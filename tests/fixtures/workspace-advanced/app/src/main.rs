use my_macro::Name;

#[derive(Name)]
struct App;

fn main() {
    let mut buf = itoa::Buffer::new();
    let answer = buf.format(42u32);
    println!(
        "name={} stamp={} answer={}",
        App::name(),
        build_info::BUILD_STAMP,
        answer,
    );
}
