extern crate compiletest_rs as compiletest;

use std::path::{PathBuf, Path};
use std::io::Write;

fn compile_fail(sysroot: &Path) {
    let flags = format!("--sysroot {} -Dwarnings", sysroot.to_str().expect("non utf8 path"));
    for_all_targets(&sysroot, |target| {
        let mut config = compiletest::default_config();
        config.host_rustcflags = Some(flags.clone());
        config.mode = "compile-fail".parse().expect("Invalid mode");
        config.run_lib_path = Path::new(sysroot).join("lib").join("rustlib").join(&target).join("lib");
        config.rustc_path = "target/debug/miri".into();
        config.src_base = PathBuf::from("tests/compile-fail".to_string());
        config.target = target.to_owned();
        config.target_rustcflags = Some(flags.clone());
        compiletest::run_tests(&config);
    });
}

fn is_target_dir<P: Into<PathBuf>>(path: P) -> bool {
    let mut path = path.into();
    path.push("lib");
    path.metadata().map(|m| m.is_dir()).unwrap_or(false)
}

fn for_all_targets<F: FnMut(String)>(sysroot: &Path, mut f: F) {
    let target_dir = sysroot.join("lib").join("rustlib");
    println!("target dir: {}", target_dir.to_str().unwrap());
    for entry in std::fs::read_dir(target_dir).expect("invalid sysroot") {
        let entry = entry.unwrap();
        if !is_target_dir(entry.path()) { continue; }
        let target = entry.file_name().into_string().unwrap();
        let stderr = std::io::stderr();
        writeln!(stderr.lock(), "running tests for target {}", target).unwrap();
        f(target);
    }
}

#[test]
fn compile_test() {
    let sysroot = std::process::Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()
        .expect("rustc not found")
        .stdout;
    let sysroot = std::str::from_utf8(&sysroot).expect("sysroot is not utf8").trim();
    let sysroot = &Path::new(&sysroot);
    compile_fail(&sysroot);
}
