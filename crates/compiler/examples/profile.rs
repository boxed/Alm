//! Temporary profiling harness: compile one entry point repeatedly in-process
//! so a sampling profiler has something to attach to.
//!
//! `ALM_PROFILE_TARGET=wasm-gc` profiles the wasm-gc pipeline instead of the JS
//! one: they share a front end but not a back end, and monomorphization runs
//! only for wasm-gc and native.
use std::path::Path;
fn main() {
    let entry = std::env::args().nth(1).expect("usage: profile <Main.elm> [runs]");
    let runs: usize = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(40);
    let path = Path::new(&entry);
    let wasm = std::env::var("ALM_PROFILE_TARGET").as_deref() == Ok("wasm-gc");
    let out = std::env::temp_dir().join("alm-profile-out.wasm");
    for i in 0..runs {
        let result = if wasm {
            alm_compiler::project::compile_project_wasmgc(path, &out, false).map(|_| String::new())
        } else {
            alm_compiler::project::compile_project_with(path, false).map(|_| String::new())
        };
        if let Err(errors) = result {
            eprintln!("failed on run {i}: {}", errors[0].render());
            std::process::exit(1);
        }
    }
}
