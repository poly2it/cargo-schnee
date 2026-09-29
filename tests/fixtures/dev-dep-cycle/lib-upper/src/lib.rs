pub fn upper(s: &str) -> String {
    let lower = lib_lower::lower(s);
    format!("{lower}!")
}
