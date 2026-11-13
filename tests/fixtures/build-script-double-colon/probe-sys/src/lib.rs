//! Items that exist only when the build script's `cargo::` directives apply.

/// Set through `cargo::rustc-env`.
pub const ENV: &str = env!("PROBE_ENV");

/// Present only under the `probe_cfg` set through `cargo::rustc-cfg`.
#[cfg(probe_cfg)]
pub const CFG: &str = "from-cfg";
