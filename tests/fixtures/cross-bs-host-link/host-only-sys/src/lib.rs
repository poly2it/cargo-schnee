extern "C" {
    fn schnee_host_only() -> i32;
}

/// Reads the value out of the host archive the build script compiles, so a
/// consumer's host link has to resolve the archive.
pub fn probe() -> i32 {
    unsafe { schnee_host_only() }
}
