use my_macro::HostProbe;

#[derive(HostProbe)]
struct App;

fn main() {
    println!("bs_target={}", App::bs_target());
}
