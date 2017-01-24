struct Foo {
    _inner: i32,
}

#[test]
fn main() {
    unsafe {
        let foo = Foo {
            _inner: std::mem::uninitialized(),
        };
        let _bar = foo;
    }
}
