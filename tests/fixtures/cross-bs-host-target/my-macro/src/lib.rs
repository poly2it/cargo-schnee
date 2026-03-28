use proc_macro::TokenStream;

/// Derive macro that embeds the BS_TARGET and BS_HOST values from host-probe.
/// Since host-probe is compiled for the host (via proc-macro dep chain),
/// BS_TARGET must equal BS_HOST. If not, the build script env is wrong.
#[proc_macro_derive(HostProbe)]
pub fn derive_host_probe(input: TokenStream) -> TokenStream {
    let src = input.to_string();
    let name = src
        .split_whitespace()
        .skip_while(|w| *w != "struct" && *w != "enum")
        .nth(1)
        .unwrap_or("Unknown")
        .trim_matches(|c: char| !c.is_alphanumeric());

    // Compile-time check: in the host variant, BS_TARGET must equal BS_HOST.
    assert_eq!(
        host_probe::BS_TARGET,
        host_probe::BS_HOST,
        "host-probe (host variant) has TARGET != HOST: TARGET={}, HOST={}",
        host_probe::BS_TARGET,
        host_probe::BS_HOST,
    );

    let bs_target = host_probe::BS_TARGET;
    let expanded = format!(
        "impl {} {{ pub fn bs_target() -> &'static str {{ \"{}\" }} }}",
        name, bs_target
    );
    expanded.parse().unwrap()
}
