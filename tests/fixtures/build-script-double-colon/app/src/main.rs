fn main() {
    println!(
        "{} {} {}",
        probe_sys::ENV,
        probe_sys::CFG,
        env!("PROBE_ROOT")
    );
}
