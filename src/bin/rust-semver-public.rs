#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_span;

use log::debug;
use rustc_driver::{Callbacks, Compilation, RunCompiler};
use rustc_interface::{interface, Queries};
use rustc_span::source_map::Pos;
use semverver::run_traversal;
use std::{
    path::Path,
    process::{exit, Command},
};

/// Display semverver version.
fn show_version() {
    println!(env!("CARGO_PKG_VERSION"));
}

/// Main routine.
///
/// Find the sysroot before passing our args to the custom compiler driver we register.
fn main() {
    rustc_driver::init_rustc_env_logger();

    debug!("running rust-semver-public compiler driver");

    exit(
        {
            use std::env;

            struct PubCallbacks;

            impl Callbacks for PubCallbacks {
                fn after_analysis<'tcx>(&mut self, _compiler: &interface::Compiler, queries: &'tcx Queries<'tcx>) -> Compilation {
                    debug!("running rust-semver-public after_analysis callback");

                    queries.global_ctxt().unwrap().peek_mut().enter(|tcx| {
                        let krate = tcx
                            .crates(())
                            .iter()
                            .flat_map(|crate_num| {
                                let def_id = crate_num.as_def_id();

                                match tcx.extern_crate(def_id) {
                                    Some(extern_crate) if extern_crate.is_direct() && extern_crate.span.data().lo.to_usize() > 0 => Some(def_id),
                                    _ => None,
                                }
                            })
                            .next();

                        if let Some(krate_def_id) = krate {
                            debug!("running semver analysis");
                            run_traversal(tcx, krate_def_id);
                        } else {
                            tcx.sess.err("could not find `new` crate");
                        }
                    });

                    debug!("rust-semver-public after_analysis callback finished!");

                    Compilation::Stop
                }
            }

            if env::args().any(|a| a == "--version" || a == "-V") {
                show_version();
                exit(0);
            }

            let sys_root = option_env!("SYSROOT")
                .map(String::from)
                .or_else(|| env::var("SYSROOT").ok())
                .or_else(|| {
                    let home = option_env!("RUSTUP_HOME").or(option_env!("MULTIRUST_HOME"));
                    let toolchain = option_env!("RUSTUP_TOOLCHAIN").or(option_env!("MULTIRUST_TOOLCHAIN"));
                    home.and_then(|home| toolchain.map(|toolchain| format!("{}/toolchains/{}", home, toolchain)))
                })
                .or_else(|| {
                    Command::new("rustc")
                        .arg("--print")
                        .arg("sysroot")
                        .output()
                        .ok()
                        .and_then(|out| String::from_utf8(out.stdout).ok())
                        .map(|s| s.trim().to_owned())
                })
                .expect("need to specify SYSROOT env var during clippy compilation, or use rustup or multirust");

            // Setting RUSTC_WRAPPER causes Cargo to pass 'rustc' as the first argument.
            // We're invoking the compiler programmatically, so we ignore this/
            let mut orig_args: Vec<String> = env::args().collect();
            if orig_args.len() <= 1 {
                std::process::exit(1);
            }

            if Path::new(&orig_args[1]).file_stem() == Some("rustc".as_ref()) {
                // we still want to be able to invoke it normally though
                orig_args.remove(1);
            }

            // this conditional check for the --sysroot flag is there so users can call
            // `clippy_driver` directly
            // without having to pass --sysroot or anything
            let args: Vec<String> = if orig_args.iter().any(|s| s == "--sysroot") {
                orig_args
            } else {
                orig_args
                    .into_iter()
                    .chain(Some("--sysroot".to_owned()))
                    .chain(Some(sys_root))
                    .collect()
            };

            RunCompiler::new(&args, &mut PubCallbacks).run()
        }
        .map_or_else(|_| 1, |_| 0),
    )
}
