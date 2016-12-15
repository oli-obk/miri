#![allow(dead_code)]
#![allow(unused)]

pub struct Foo {
    inner: i32,
}

fn main() {
    unsafe {
        let foo = Foo {
            inner: std::mem::uninitialized(),
        };
        let bar = foo;
    }
}
