extern crate compiletest_rs as compiletest;
extern crate rustc_tests;

use rustc_tests::get_sysroot;

use std::path::{PathBuf, Path};
use std::io::Write;

macro_rules! eprintln {
    ($($arg:tt)*) => {
        let stderr = std::io::stderr();
        writeln!(stderr.lock(), $($arg)*).unwrap();
    }
}

fn compile_fail(sysroot: &Path, path: &str, target: &str, host: &str, fullmir: bool) {
    eprintln!("## Running compile-fail tests in {} against miri for target {}", path, target);
    // if we are building as part of the rustc test suite, we already have fullmir for everything
    let sysroot = if fullmir && option_env!("RUSTC_TEST_SUITE").map_or(true, |env| env != "1") {
        if host != target {
            // skip fullmir on nonhost
            return;
        }
        Path::new(&std::env::var("HOME").unwrap()).join(".xargo").join("HOST")
    } else {
        sysroot.to_owned()
    };
    if target == host {
        std::env::set_var("MIRI_HOST_TARGET", "yes");
    }
    let report = rustc_tests::run(path, &sysroot, 0).unwrap_err();
    assert_eq!(report.success, 0);
    std::env::set_var("MIRI_HOST_TARGET", "");
}

fn run_pass(path: &str) {
    eprintln!("## Running run-pass tests in {} against rustc", path);
    let mut config = compiletest::default_config();
    config.mode = "run-pass".parse().expect("Invalid mode");
    config.src_base = PathBuf::from(path);
    config.target_rustcflags = Some("-Dwarnings".to_string());
    config.host_rustcflags = Some("-Dwarnings".to_string());
    compiletest::run_tests(&config);
}

fn miri_pass(path: &str, target: &str, host: &str, fullmir: bool, opt: bool) {
    let opt_str = if opt {
        " with optimizations"
    } else {
        ""
    };
    eprintln!("## Running run-pass tests in {} against miri for target {}{}", path, target, opt_str);
    // if we are building as part of the rustc test suite, we already have fullmir for everything
    let sysroot = if fullmir && option_env!("RUSTC_TEST_SUITE").map_or(true, |env| env != "1") {
        if host != target {
            // skip fullmir on nonhost
            return;
        }
        Path::new(&std::env::var("HOME").unwrap()).join(".xargo").join("HOST")
    } else {
        get_sysroot()
    };
    let opt_level = if opt {
        3
    } else {
        0
    };
    if target == host {
        std::env::set_var("MIRI_HOST_TARGET", "yes");
    }
    rustc_tests::run(path, &sysroot, opt_level).unwrap();
    std::env::set_var("MIRI_HOST_TARGET", "");
}

fn is_target_dir<P: Into<PathBuf>>(path: P) -> bool {
    let mut path = path.into();
    path.push("lib");
    path.metadata().map(|m| m.is_dir()).unwrap_or(false)
}

fn for_all_targets<F: FnMut(String)>(sysroot: &Path, mut f: F) {
    let target_dir = sysroot.join("lib").join("rustlib");
    for entry in std::fs::read_dir(target_dir).expect("invalid sysroot") {
        let entry = entry.unwrap();
        if !is_target_dir(entry.path()) { continue; }
        let target = entry.file_name().into_string().unwrap();
        f(target);
    }
}

fn get_host() -> String {
    let host = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("rustc not found for -vV")
        .stdout;
    let host = std::str::from_utf8(&host).expect("sysroot is not utf8");
    let host = host.split("\nhost: ").nth(1).expect("no host: part in rustc -vV");
    let host = host.split('\n').next().expect("no \n after host");
    String::from(host)
}

#[test]
fn run_pass_miri() {
    let sysroot = get_sysroot();
    let host = get_host();

    for &opt in [false, true].iter() {
        for_all_targets(&sysroot, |target| {
            miri_pass("tests/run-pass", &target, &host, false, opt);
        });
        miri_pass("tests/run-pass-fullmir", &host, &host, true, opt);
    }
}

#[test]
fn run_pass_rustc() {
    run_pass("tests/run-pass");
    run_pass("tests/run-pass-fullmir");
}

#[test]
fn compile_fail_miri() {
    let sysroot = get_sysroot();
    let host = get_host();

    for_all_targets(&sysroot, |target| {
        compile_fail(&sysroot, "tests/compile-fail", &target, &host, false);
    });
    compile_fail(&sysroot, "tests/compile-fail-fullmir", &host, &host, true);
}
