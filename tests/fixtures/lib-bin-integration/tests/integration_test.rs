use lib_bin_integration::greet;

#[test]
fn test_greet() {
    assert_eq!(greet(), "hello from lib");
}
