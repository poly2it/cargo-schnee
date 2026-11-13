fn main() {
    pkg_config::probe_library("openssl").expect("pkg-config finds openssl");
}
