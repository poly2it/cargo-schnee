extern "C" {
    fn nativetestlib_hello() -> i32;
}

pub fn hello() -> i32 {
    unsafe { nativetestlib_hello() }
}
