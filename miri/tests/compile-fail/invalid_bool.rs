fn main() {
    let b = unsafe { std::mem::transmute::<u8, bool>(2) };
    if b { unreachable!() } else { unreachable!() } //~ ERROR: invalid boolean value read
}
