use proc_macro::TokenStream;

/// A simple derive macro that implements a `name()` method returning the struct name.
#[proc_macro_derive(Name)]
pub fn derive_name(input: TokenStream) -> TokenStream {
    let src = input.to_string();
    // Naive parse: find "struct Foo" or "enum Foo"
    let name = src
        .split_whitespace()
        .skip_while(|w| *w != "struct" && *w != "enum")
        .nth(1)
        .unwrap_or("Unknown")
        .trim_matches(|c: char| !c.is_alphanumeric());

    let expanded = format!(
        "impl {} {{ pub fn name() -> &'static str {{ \"{}\" }} }}",
        name, name
    );
    expanded.parse().unwrap()
}
