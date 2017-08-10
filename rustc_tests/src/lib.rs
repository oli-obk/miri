#![feature(rustc_private, i128_type)]
extern crate miri;
extern crate getopts;
extern crate rustc;
extern crate rustc_driver;
extern crate rustc_errors;
extern crate syntax;

use std::path::{PathBuf, Path};
use std::io::Write;
use std::sync::{Mutex, Arc};
use std::io;
use std::collections::HashMap;
use std::ffi::OsStr;


use rustc::session::Session;
use rustc_driver::{Compilation, CompilerCalls, RustcDefaultCalls};
use rustc_driver::driver::{CompileState, CompileController};
use rustc::session::config::{self, Input, ErrorOutputType};
use rustc::hir::{self, itemlikevisit};
use rustc::ty::TyCtxt;
use syntax::ast;

struct MiriCompilerCalls(RustcDefaultCalls);

impl<'a> CompilerCalls<'a> for MiriCompilerCalls {
    fn early_callback(
        &mut self,
        matches: &getopts::Matches,
        sopts: &config::Options,
        cfg: &ast::CrateConfig,
        descriptions: &rustc_errors::registry::Registry,
        output: ErrorOutputType
    ) -> Compilation {
        self.0.early_callback(matches, sopts, cfg, descriptions, output)
    }
    fn no_input(
        &mut self,
        matches: &getopts::Matches,
        sopts: &config::Options,
        cfg: &ast::CrateConfig,
        odir: &Option<PathBuf>,
        ofile: &Option<PathBuf>,
        descriptions: &rustc_errors::registry::Registry
    ) -> Option<(Input, Option<PathBuf>)> {
        self.0.no_input(matches, sopts, cfg, odir, ofile, descriptions)
    }
    fn late_callback(
        &mut self,
        matches: &getopts::Matches,
        sess: &Session,
        input: &Input,
        odir: &Option<PathBuf>,
        ofile: &Option<PathBuf>
    ) -> Compilation {
        self.0.late_callback(matches, sess, input, odir, ofile)
    }
    fn build_controller(&mut self, sess: &Session, matches: &getopts::Matches) -> CompileController<'a> {
        let mut control = self.0.build_controller(sess, matches);
        control.after_hir_lowering.callback = Box::new(after_hir_lowering);
        control.after_analysis.callback = Box::new(after_analysis);
        if std::env::var("MIRI_HOST_TARGET") != Ok("yes".to_owned()) {
            // only fully compile targets on the host
            control.after_analysis.stop = Compilation::Stop;
        }
        control
    }
}

fn after_hir_lowering(state: &mut CompileState) {
    let attr = (String::from("miri"), syntax::feature_gate::AttributeType::Whitelisted);
    state.session.plugin_attributes.borrow_mut().push(attr);
}

fn after_analysis<'a, 'tcx>(state: &mut CompileState<'a, 'tcx>) {
    state.session.abort_if_errors();

    let tcx = state.tcx.unwrap();
    let limits = Default::default();

    if std::env::args().any(|arg| arg == "--test") {
        struct Visitor<'a, 'tcx: 'a>(miri::ResourceLimits, TyCtxt<'a, 'tcx, 'tcx>, &'a CompileState<'a, 'tcx>);
        impl<'a, 'tcx: 'a, 'hir> itemlikevisit::ItemLikeVisitor<'hir> for Visitor<'a, 'tcx> {
            fn visit_item(&mut self, i: &'hir hir::Item) {
                if let hir::Item_::ItemFn(_, _, _, _, _, body_id) = i.node {
                    if i.attrs.iter().any(|attr| attr.name().map_or(false, |n| n == "test")) {
                        let did = self.1.hir.body_owner_def_id(body_id);
                        println!("running test: {}", self.1.hir.def_path(did).to_string(self.1));
                        miri::eval_main(self.1, did, None, self.0);
                        self.2.session.abort_if_errors();
                    }
                }
            }
            fn visit_trait_item(&mut self, _trait_item: &'hir hir::TraitItem) {}
            fn visit_impl_item(&mut self, _impl_item: &'hir hir::ImplItem) {}
        }
        state.hir_crate.unwrap().visit_all_item_likes(&mut Visitor(limits, tcx, state));
    } else if let Some((entry_node_id, _)) = *state.session.entry_fn.borrow() {
        let entry_def_id = tcx.hir.local_def_id(entry_node_id);
        let start_wrapper = tcx.lang_items.start_fn().and_then(|start_fn|
                                if tcx.is_mir_available(start_fn) { Some(start_fn) } else { None });
        miri::eval_main(tcx, entry_def_id, start_wrapper, limits);

        state.session.abort_if_errors();
    } else {
        println!("no main function found, assuming auxiliary build");
    }
}

pub fn run<
    P: AsRef<OsStr> + ?Sized,
    Q: AsRef<OsStr> + ?Sized,
>(
    path: &P,
    sysroot: &Q,
    opt_level: usize,
) -> Result<u64, Report> {
    run_inner(Path::new(path), Path::new(sysroot), opt_level)
}

pub fn get_sysroot() -> PathBuf {
    let sysroot = std::env::var("MIRI_SYSROOT").unwrap_or_else(|_| {
        let sysroot = std::process::Command::new("rustc")
            .arg("--print")
            .arg("sysroot")
            .output()
            .expect("rustc not found")
            .stdout;
        String::from_utf8(sysroot).expect("sysroot is not utf8")
    });
    PathBuf::from(sysroot.trim())
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct Report {
    /// The number of successful tests
    pub success: u64,
    pub mir_not_found: HashMap<String, usize>,
    pub crate_not_found: HashMap<String, usize>,
    /// Generic failure
    pub failed: HashMap<String, usize>,
    pub c_abi_fns: HashMap<String, usize>,
    pub abi: HashMap<String, usize>,
    pub unsupported: HashMap<String, usize>,
    pub unimplemented_intrinsic: HashMap<String, usize>,
    pub limits: HashMap<String, usize>,
}

trait HashMapInc {
    fn inc(&mut self, s: &str);
}

impl HashMapInc for HashMap<String, usize> {
    fn inc(&mut self, s: &str) {
        *self.entry(s.to_owned()).or_insert(0) += 1;
    }
}

fn run_inner(path: &Path, sysroot: &Path, opt_level: usize) -> Result<u64, Report> {
    let mut report = Report::default();
    let mut files: Vec<_> = std::fs::read_dir(path).unwrap().collect();
    while let Some(file) = files.pop() {
        let file = file.unwrap();
        let path = file.path();
        if file.metadata().unwrap().is_dir() {
            if !path.to_str().unwrap().ends_with("auxiliary") {
                // add subdirs recursively
                files.extend(std::fs::read_dir(path).unwrap());
            }
            continue;
        }
        if !file.metadata().unwrap().is_file() || !path.to_str().unwrap().ends_with(".rs") {
            continue;
        }
        let stderr = std::io::stderr();
        write!(stderr.lock(), "test [miri-pass] {} ... ", path.display()).unwrap();
        let mut args = Vec::new();
        args.push("miri".to_string());
        // file to process
        args.push(path.display().to_string());

        // parse test specific compile flags
        use std::io::{BufReader, BufRead};
        let mut aux = Vec::new();
        for line in BufReader::new(std::fs::File::open(path).unwrap()).lines() {
            let mut line = &*line.unwrap();
            if line.starts_with("//") {
                line = line[2..].trim();
            } else {
                continue;
            }
            if line.starts_with("compile-flags:") {
                line = line["compile-flags:".len()..].trim();
                args.extend(line.split_whitespace().map(ToString::to_string));
            } else if line.starts_with("aux-build:") {
                line = line["aux-build:".len()..].trim();
                aux.extend(line.split(',').map(|s| s.trim().to_string()));
            }
        }

        let sysroot_flag = String::from("--sysroot");
        args.push(sysroot_flag);
        args.push(sysroot.display().to_string());

        if !args.iter().any(|arg| arg.starts_with("-Zmir-opt-level=")) {
            args.push(format!("-Zmir-opt-level={}", opt_level));
        }
        // for auxilary builds in unit tests
        args.push("-Zalways-encode-mir".to_owned());
        if opt_level == 0 && !args.iter().any(|arg| arg.starts_with("-Zmir-emit-validate=")) {
            // For now, only validate without optimizations.  Inlining breaks validation.
            args.push("-Zmir-emit-validate=1".to_owned());
        }

        // A threadsafe buffer for writing.
        #[derive(Default, Clone)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.0.lock().unwrap().flush()
            }
        }
        let buf = BufWriter::default();
        let output = buf.clone();
        let result = std::panic::catch_unwind(|| {
            rustc_driver::run_compiler(&args, &mut MiriCompilerCalls(RustcDefaultCalls), None, Some(Box::new(buf)));
        });

        match result {
            Ok(()) => {
                report.success += 1;
                writeln!(stderr.lock(), "ok").unwrap()
            },
            Err(_) => {
                let output = output.0.lock().unwrap();
                let output_err = std::str::from_utf8(&output).unwrap();
                if let Some(text) = output_err.splitn(2, "no mir for `").nth(1) {
                    let end = text.find('`').unwrap();
                    report.mir_not_found.inc(&text[..end]);
                    writeln!(stderr.lock(), "NO MIR FOR `{}`", &text[..end]).unwrap();
                } else if let Some(text) = output_err.splitn(2, "can't find crate for `").nth(1) {
                    let end = text.find('`').unwrap();
                    report.crate_not_found.inc(&text[..end]);
                    writeln!(stderr.lock(), "CAN'T FIND CRATE FOR `{}`", &text[..end]).unwrap();
                } else {
                    for text in output_err.split("error: ").skip(1) {
                        let end = text.find('\n').unwrap_or(text.len());
                        let c_abi = "can't call C ABI function: ";
                        let unimplemented_intrinsic_s = "unimplemented intrinsic: ";
                        let unsupported_s = "miri does not support ";
                        let abi_s = "can't handle function with ";
                        let limit_s = "reached the configured maximum ";
                        if text.starts_with(c_abi) {
                            report.c_abi_fns.inc(&text[c_abi.len()..end]);
                        } else if text.starts_with(unimplemented_intrinsic_s) {
                            report.unimplemented_intrinsic.inc(&text[unimplemented_intrinsic_s.len()..end]);
                        } else if text.starts_with(unsupported_s) {
                            report.unsupported.inc(&text[unsupported_s.len()..end]);
                        } else if text.starts_with(abi_s) {
                            report.abi.inc(&text[abi_s.len()..end]);
                        } else if text.starts_with(limit_s) {
                            report.limits.inc(&text[limit_s.len()..end]);
                        } else if text.find("aborting").is_none() {
                            report.failed.inc(&text[..end]);
                        }
                    }
                    writeln!(stderr.lock(), "stderr: \n {}", output_err).unwrap();
                }
            }
        }
    }
    // I don't want to check all fields, so just check whether the report differs from the default
    // except for the success field of course.
    let success = report.success;
    report.success = 0;
    if report == Report::default() {
        Ok(success)
    } else {
        report.success = success;
        Err(report)
    }
}
