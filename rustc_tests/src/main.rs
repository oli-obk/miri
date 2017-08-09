extern crate rustc_tests;

use std::io::Write;
use std::collections::HashMap;

fn main() {
    let path = std::env::args().skip(1).next().unwrap();

    let sysroot = rustc_tests::get_sysroot();

    let rustc_tests::Report {
        success,
        mir_not_found,
        crate_not_found,
        failed,
        c_abi_fns,
        abi,
        unsupported,
        unimplemented_intrinsic,
        limits,
    } = rustc_tests::run(&path, &sysroot, 3).unwrap_err();
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    writeln!(stderr, "{} success, {} no mir, {} crate not found, {} failed, \
                        {} C fn, {} ABI, {} unsupported, {} intrinsic",
                        success, mir_not_found.len(), crate_not_found.len(), failed.len(),
                        c_abi_fns.len(), abi.len(), unsupported.len(), unimplemented_intrinsic.len()).unwrap();
    writeln!(stderr, "# The \"other reasons\" errors").unwrap();
    writeln!(stderr, "(sorted, deduplicated)").unwrap();
    print_vec(&mut stderr, failed);

    writeln!(stderr, "# can't call C ABI function").unwrap();
    print_vec(&mut stderr, c_abi_fns);

    writeln!(stderr, "# unsupported ABI").unwrap();
    print_vec(&mut stderr, abi);

    writeln!(stderr, "# unsupported").unwrap();
    print_vec(&mut stderr, unsupported);

    writeln!(stderr, "# unimplemented intrinsics").unwrap();
    print_vec(&mut stderr, unimplemented_intrinsic);

    writeln!(stderr, "# mir not found").unwrap();
    print_vec(&mut stderr, mir_not_found);

    writeln!(stderr, "# crate not found").unwrap();
    print_vec(&mut stderr, crate_not_found);

    writeln!(stderr, "# miri limits").unwrap();
    print_vec(&mut stderr, limits);
}

fn print_vec<W: std::io::Write>(stderr: &mut W, v: HashMap<String, usize>) {
    writeln!(stderr, "```").unwrap();
    for (s, n) in v {
        writeln!(stderr, "{:4} {}", n, s).unwrap();
    }
    writeln!(stderr, "```").unwrap();
}
