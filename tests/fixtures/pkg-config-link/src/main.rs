unsafe extern "C" {
    fn OpenSSL_version_num() -> std::ffi::c_ulong;
}

fn main() {
    let version = unsafe { OpenSSL_version_num() };
    println!("openssl={version:x}");
}
