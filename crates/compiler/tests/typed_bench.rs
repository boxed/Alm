//! Profiling harness (ignored by default): compare the typed/unboxed backend
//! against the uniform native backend and node on a scalar-heavy workload.
//!
//!   cargo test -p alm-compiler --test typed_bench -- --ignored --nocapture

use std::process::Command;
use std::time::Instant;

use alm_compiler::generate::native::{self, Target};
use alm_compiler::generate::typed;
use alm_compiler::interface::Interfaces;
use alm_compiler::ir::layout::LayoutCtx;
use alm_compiler::ir::{lower, mono};
use alm_compiler::{canonicalize, generate, parse, typecheck};

const FIB: &str = "module Test exposing (..)\n\
     \n\
     fib : Int -> Int\n\
     fib n =\n\
     \x20   if n <= 1 then n else fib (n - 1) + fib (n - 2)\n\
     \n\
     main : Int\n\
     main = fib 33\n";

fn best_ms(cmd: &mut Command, runs: u32) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        let out = cmd.output().expect("run");
        assert!(out.status.success(), "run failed");
        best = best.min(start.elapsed().as_secs_f64() * 1000.0);
    }
    best
}

#[test]
#[ignore]
fn bench_fib() {
    let dir = std::env::temp_dir().join(format!("alm-typed-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let module = parse::parse_module(FIB).unwrap();
    let canonical = canonicalize::canonicalize(&module).unwrap();
    let checked = typecheck::check_module(&canonical, &Interfaces::new()).unwrap();

    // JS
    let js = generate::generate_project(std::slice::from_ref(&canonical));
    let bundle = dir.join("b.js");
    std::fs::write(&bundle, js).unwrap();
    let mut node = Command::new("node");
    node.arg("-e").arg(format!(
        "console.log(require({:?})['Test']['main'])",
        bundle.display()
    ));

    // Uniform native
    let program = lower::lower_project(std::slice::from_ref(&canonical));
    let uni = dir.join("uniform");
    native::build(&program, &uni, Target::Native).unwrap();

    // Typed native
    let mono = mono::specialize_program(&canonical, &checked.types, &checked.node_types);
    let layouts = LayoutCtx::new(&canonical);
    let typ = dir.join("typed");
    typed::build(&mono, &layouts, &typ, Target::Native).unwrap();

    // Correctness first: all three agree.
    let expect = String::from_utf8_lossy(&node.output().unwrap().stdout)
        .trim_end()
        .to_string();
    for (label, path) in [("uniform", &uni), ("typed", &typ)] {
        let got = String::from_utf8_lossy(&Command::new(path).output().unwrap().stdout)
            .trim_end()
            .to_string();
        assert_eq!(got, expect, "{} disagrees (got {} want {})", label, got, expect);
    }

    let runs = 5;
    let node_ms = best_ms(&mut node, runs);
    let uni_ms = best_ms(&mut Command::new(&uni), runs);
    let typ_ms = best_ms(&mut Command::new(&typ), runs);

    println!("\nfib 33  (result {})", expect);
    println!("  node          {:>8.1} ms", node_ms);
    println!("  uniform native{:>8.1} ms  ({:.2}x vs node)", uni_ms, node_ms / uni_ms);
    println!("  typed native  {:>8.1} ms  ({:.2}x vs node, {:.2}x vs uniform)", typ_ms, node_ms / typ_ms, uni_ms / typ_ms);
}
