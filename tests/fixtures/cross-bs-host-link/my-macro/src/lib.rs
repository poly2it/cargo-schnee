use proc_macro::TokenStream;

/// Expands to the value the host archive returns, which forces this
/// proc-macro's own host link to resolve that archive.
#[proc_macro]
pub fn host_probe(_input: TokenStream) -> TokenStream {
    format!("pub const HOST_PROBE: i32 = {};", host_only_sys::probe())
        .parse()
        .unwrap()
}
