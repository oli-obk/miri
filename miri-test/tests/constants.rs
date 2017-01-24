const A: usize = *&5;

fn foo() -> usize {
    A
}

#[test]
fn main() {
    assert_eq!(foo(), A);
}
