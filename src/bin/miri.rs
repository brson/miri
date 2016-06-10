#![feature(rustc_private, custom_attribute)]
#![allow(unused_attributes)]

extern crate getopts;
extern crate miri;
extern crate rustc;
extern crate rustc_driver;
extern crate env_logger;
extern crate log_settings;
extern crate log;

use miri::interpreter;
use rustc::session::Session;
use rustc_driver::{driver, CompilerCalls};

struct MiriCompilerCalls;

impl<'a> CompilerCalls<'a> for MiriCompilerCalls {
    fn build_controller(
        &mut self,
        _: &Session,
        _: &getopts::Matches
    ) -> driver::CompileController<'a> {
        let mut control = driver::CompileController::basic();

        control.after_analysis.callback = Box::new(|state| {
            state.session.abort_if_errors();
            interpreter::interpret_start_points(state.tcx.unwrap(), state.mir_map.unwrap());
        });

        control
    }
}

#[miri_run]
fn main() {
    init_logger();
    let args: Vec<String> = std::env::args().collect();
    rustc_driver::run_compiler(&args, &mut MiriCompilerCalls);
}

#[miri_run]
fn init_logger() {
    const NSPACES: usize = 40;
    let format = |record: &log::LogRecord| {
        // prepend spaces to indent the final string
        let indentation = log_settings::settings().indentation;
        format!("{lvl}:{module}{depth:2}{indent:<indentation$} {text}",
            lvl = record.level(),
            module = record.location().module_path(),
            depth = indentation / NSPACES,
            indentation = indentation % NSPACES,
            indent = "",
            text = record.args())
    };

    let mut builder = env_logger::LogBuilder::new();
    builder.format(format).filter(None, log::LogLevelFilter::Info);

    if std::env::var("MIRI_LOG").is_ok() {
        builder.parse(&std::env::var("MIRI_LOG").unwrap());
    }

    builder.init().unwrap();
}