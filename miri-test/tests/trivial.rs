fn empty() {}

fn unit_var() {
    let x = ();
    x
}

#[test]
fn main() {
    empty();
    unit_var();
}
