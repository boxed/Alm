//! Profiling harness (ignored by default): compare the typed/unboxed backend
//! against the uniform native backend and node on a scalar-heavy workload.
//!
//!   cargo test -p alm-compiler --test typed_bench -- --ignored --nocapture


mod common;

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

// A tail-recursive record accumulator: the uniform backend allocates a
// fresh record each iteration, the typed backend keeps it in registers.
const ACC: &str = "module Test exposing (..)\n\
     \n\
     type alias Acc = { sum : Int, cnt : Int }\n\
     \n\
     step : Int -> Acc -> Acc\n\
     step n a =\n\
     \x20   if n <= 0 then a else step (n - 1) { sum = a.sum + n, cnt = a.cnt + 1 }\n\
     \n\
     main : Int\n\
     main =\n\
     \x20   let\n\
     \x20       a = step 10000000 { sum = 0, cnt = 0 }\n\
     \x20   in\n\
     \x20   a.sum + a.cnt\n";

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

// A list pipeline: range -> map -> foldl. The typed backend emits inline
// unboxed loops calling the specialized functions; the uniform backend boxes
// every element and closure-applies each step. Sum of 2n for 1..1e6 ~ 1e12,
// under 2^53 so JS's f64 stays exact.
const LIST: &str = "module Test exposing (..)\n\
     \n\
     double : Int -> Int\n\
     double n = n + n\n\
     \n\
     add : Int -> Int -> Int\n\
     add x acc = x + acc\n\
     \n\
     main : Int\n\
     main =\n\
     \x20   List.range 1 1000000\n\
     \x20       |> List.map double\n\
     \x20       |> List.foldl add 0\n";

#[test]
#[ignore]
fn bench() {
    bench_one("fib 33", FIB);
    bench_one("record-acc 10M", ACC);
    bench_one("list range|>map|>foldl 1M", LIST);
}

fn bench_one(label: &str, src: &str) {
    let dir = common::test_dir("alm-typed-bench", &label.replace(' ', "_"));

    let module = parse::parse_module(src).unwrap();
    let canonical = canonicalize::canonicalize(&module).unwrap();
    let checked = typecheck::check_module(&canonical, &Interfaces::new()).unwrap();

    // JS
    let mut nt = std::collections::HashMap::new();
    nt.insert(canonical.name.clone(), checked.node_types.clone());
    let js = generate::generate_project_typed(std::slice::from_ref(&canonical), nt, true);
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
    typed::build(&mono, &layouts, &typ, Target::Native, std::collections::HashMap::new()).unwrap();

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

    println!("\n{}  (result {})", label, expect);
    println!("  node          {:>8.1} ms", node_ms);
    println!("  uniform native{:>8.1} ms  ({:.2}x vs node)", uni_ms, node_ms / uni_ms);
    println!("  typed native  {:>8.1} ms  ({:.2}x vs node, {:.2}x vs uniform)", typ_ms, node_ms / typ_ms, uni_ms / typ_ms);
}
