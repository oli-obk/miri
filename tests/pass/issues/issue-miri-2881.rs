use std::cell::Cell;

pub fn main() {
    let a = 123;
    let b = Box::new(a);
    let r = Box::leak(b);

    thread_local! {
        static REF: Cell<Option<&'static i32>> = Cell::new(None);
    }

    REF.with(|cell| {
        cell.set(Some(r));
    })
}
