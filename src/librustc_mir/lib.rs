#![feature(
    i128_type,
    rustc_private,
)]

// From rustc.
#[macro_use]
extern crate log;
extern crate log_settings;
#[macro_use]
extern crate rustc;
extern crate rustc_const_math;
extern crate rustc_data_structures;
extern crate syntax;

// From crates.io.
extern crate byteorder;

pub mod interpret;
