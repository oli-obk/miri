// This tests that the size of Option<Box<i32>> is the same as *const i32.
fn option_box_deref() -> i32 {
    let val = Some(Box::new(42));
    unsafe {
        let ptr: *const i32 = std::mem::transmute::<Option<Box<i32>>, *const i32>(val);
        *ptr
    }
}

#[test]
fn main() {
    assert_eq!(option_box_deref(), 42);
}
