static X: usize = 5;

#[allow(mutable_transmutes)]
fn main() {
    unsafe {
        *std::mem::transmute::<&usize, &mut usize>(&X) = 6; //~ ERROR: modified memory
        assert_eq!(X, 6);
    }
}
