struct Foo;

static mut COUNTER: usize = 0;

impl Drop for Foo {
    fn drop(&mut self) {
        unsafe { COUNTER += 1}
    }
}

#[allow(dead_code)]
struct Bar {
    x: usize,
    y: Result<Box<usize>, Foo>,
}

fn main() {
    assert_eq!(std::mem::size_of::<Result<Box<usize>, Foo>>(), std::mem::size_of::<usize>());
    assert_eq!(unsafe{COUNTER}, 0);
    drop(Ok::<Box<usize>, Foo>(Box::new(42)));
    assert_eq!(unsafe{COUNTER}, 0);
    drop(Err::<Box<usize>, Foo>(Foo));
    assert_eq!(unsafe{COUNTER}, 1);
    drop(Bar {
        x: 42,
        y: Ok(Box::new(44)),
    });
    assert_eq!(unsafe{COUNTER}, 1);
    drop(Bar {
        x: 42,
        y: Err(Foo),
    });
    assert_eq!(unsafe{COUNTER}, 2);
}
