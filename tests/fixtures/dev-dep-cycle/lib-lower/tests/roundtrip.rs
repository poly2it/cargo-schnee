// Integration test that exercises the dev-dep edge back to lib-upper.
// Without the dev-dep, the cycle case can't manifest.

#[test]
fn roundtrip_through_upper() {
    let out = lib_upper::upper("Hello");
    assert_eq!(out, "hello!");
}
