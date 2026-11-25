//! `psync-driver`: rustc with psync's effect analysis, run by `cargo psync check` as
//! `RUSTC_WRAPPER`. For every crate it compiles, it writes effect summaries of the crate's
//! functions; in crates with `#[psync::graph]` rewrites, it also checks them.
#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

// Override the C allocator in the same way that the `rustc` binary would do.
rustc_driver::override_c_allocator_in_binary!();

#[path = "../driver/analysis.rs"]
mod analysis;

use rustc_driver::Compilation;
use rustc_middle::ty::TyCtxt;
use rustc_session::EarlyDiagCtxt;
use rustc_session::config::ErrorOutputType;
use std::process::ExitCode;

/// Plain rustc, for cargo's queries (`-vV`, `--print`).
struct Plain;
impl rustc_driver::Callbacks for Plain {}

struct Psync;
impl rustc_driver::Callbacks for Psync {
    fn config(&mut self, config: &mut rustc_interface::interface::Config) {
        // Recheck a crate when the linked resolution of `dyn` calls changes: cargo reruns a crate
        // whose recorded environment dependencies changed.
        config.track_state = Some(Box::new(|sess| {
            // Only the user's crates use the linked resolution; crates.io and git crates do not
            // need recompiling when it changes.
            let source = sess
                .local_crate_source_file()
                .and_then(|f| f.local_path().map(|p| p.display().to_string()))
                .unwrap_or_default();
            if source.contains("/.cargo/registry/") || source.contains("/.cargo/git/") {
                return;
            }
            let value = std::env::var("PSYNC_LINKED_STAMP").ok();
            sess.env_depinfo.borrow_mut().insert((
                rustc_span::Symbol::intern("PSYNC_LINKED_STAMP"),
                value.as_deref().map(rustc_span::Symbol::intern),
            ));
        }));
    }

    fn after_analysis<'tcx>(
        &mut self,
        _compiler: &rustc_interface::interface::Compiler,
        tcx: TyCtxt<'tcx>,
    ) -> Compilation {
        let mut analyzer = analysis::Analyzer::new(tcx);
        let mut checker = analysis::Checker::new(&mut analyzer);
        checker.run();
        checker.write_findings();
        analyzer.write_summaries();
        Compilation::Continue
    }
}

fn main() -> ExitCode {
    let early_dcx = EarlyDiagCtxt::new(ErrorOutputType::default());
    rustc_driver::init_rustc_env_logger(&early_dcx);
    rustc_driver::install_ice_hook("https://github.com/sahastasai/psync/issues", |_| ());
    rustc_driver::catch_with_exit_code(move || {
        let mut args = rustc_driver::args::raw_args(&early_dcx);
        // As `RUSTC_WRAPPER`, the second argument is the rustc cargo would have run.
        if args.get(1).is_some_and(|a| {
            std::path::Path::new(a)
                .file_stem()
                .is_some_and(|s| s == "rustc")
        }) {
            args.remove(1);
        }
        if !args
            .iter()
            .any(|a| a == "--sysroot" || a.starts_with("--sysroot="))
        {
            args.extend(["--sysroot".to_string(), env!("PSYNC_SYSROOT").to_string()]);
        }
        let query = args
            .iter()
            .any(|a| a == "-vV" || a == "-V" || a.starts_with("--print"));
        if query {
            rustc_driver::compiler_entrypoint(&args, &mut Plain);
        } else {
            rustc_driver::compiler_entrypoint(&args, &mut Psync);
        }
        ExitCode::SUCCESS
    })
}
