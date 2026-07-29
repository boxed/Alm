//! Temporary profiling harness: compile one entry point repeatedly in-process
//! so a sampling profiler has something to attach to.
use std::path::Path;
fn main() {
    let entry = std::env::args().nth(1).expect("usage: profile <Main.elm> [runs]");
    let runs: usize = std::env::args().nth(2).and_then(|n| n.parse().ok()).unwrap_or(40);
    let path = Path::new(&entry);
    for i in 0..runs {
        match alm_compiler::project::compile_project_with(path, false) {
            Ok(_) => {}
            Err(errors) => {
                eprintln!("failed on run {i}: {}", errors[0].render());
                std::process::exit(1);
            }
        }
    }
}
