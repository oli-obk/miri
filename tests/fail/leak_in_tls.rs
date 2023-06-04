//@error-in-other-file: memory leaked
//@normalize-stderr-test: ".*│.*" -> "$$stripped$$"

use std::cell::Cell;

pub fn main() {
    thread_local! {
        static REF: Cell<Option<&'static i32>> = Cell::new(None);
    }

    std::thread::spawn(|| {
        REF.with(|cell| {
            let a = 123;
            let b = Box::new(a);
            let r = Box::leak(b);
            cell.set(Some(r));
        })
    })
    .join()
    .unwrap();

    // Imagine the program running for a long time while the thread is gone
    // and this memory still sits around, unused -- leaked.
}
