//! End-to-end tests for the typed (monomorphized) native backend. Compile a
//! scalar program to an unboxed native binary, run it, and check the output
//! against the JS backend under node — the same differential discipline the
//! uniform native backend uses.

use std::process::Command;

use alm_compiler::generate::native::Target;
use alm_compiler::generate::typed;
use alm_compiler::ir::layout::LayoutCtx;
use alm_compiler::ir::mono;
use alm_compiler::interface::Interfaces;
use alm_compiler::{canonicalize, generate, parse, typecheck};

fn run_both(test_name: &str, source: &str) -> (String, String) {
    let dir = std::env::temp_dir()
        .join("alm-typed-tests")
        .join(format!("{}-{}", test_name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");

    let module = parse::parse_module(source).expect("parse");
    let canonical = canonicalize::canonicalize(&module).expect("canonicalize");
    let interfaces = Interfaces::new();
    let checked = typecheck::check_module(&canonical, &interfaces).expect("check");

    // JS backend, via node.
    let js = generate::generate_project(std::slice::from_ref(&canonical));
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, js).expect("write bundle");
    let js_out = run(Command::new("node").arg("-e").arg(format!(
        "console.log(require({:?})['Test']['main'])",
        bundle.display()
    )));

    // Typed native backend.
    let program = mono::specialize_program(&canonical, &checked.types, &checked.node_types);
    let layouts = LayoutCtx::new(&canonical);
    let binary = dir.join(test_name);
    typed::build(&program, &layouts, &binary, Target::Native)
        .unwrap_or_else(|e| panic!("typed build failed: {}", e));
    let native_out = run(&mut Command::new(&binary));

    (js_out, native_out)
}

fn run(command: &mut Command) -> String {
    let output = command.output().expect("spawn");
    assert!(
        output.status.success(),
        "failed with {:?}:\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

fn assert_same(test_name: &str, source: &str) {
    let (js, native) = run_both(test_name, source);
    assert!(!js.is_empty(), "JS output is empty");
    assert_eq!(native, js, "typed native and JS backends disagree");
}

#[test]
fn integer_arithmetic() {
    assert_same(
        "int_arith",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main = 2 * 21 + 100 - 42\n",
    );
}

#[test]
fn calls_and_recursion() {
    assert_same(
        "recursion",
        "module Test exposing (..)\n\
         \n\
         double : Int -> Int\n\
         double n = n + n\n\
         \n\
         sumTo : Int -> Int\n\
         sumTo n =\n\
         \x20   if n <= 0 then 0 else n + sumTo (n - 1)\n\
         \n\
         main : Int\n\
         main = double (sumTo 10)\n",
    );
}

#[test]
fn float_arithmetic() {
    assert_same(
        "float_arith",
        "module Test exposing (..)\n\
         \n\
         main : Float\n\
         main = 3.0 * 2.5 + 1.0\n",
    );
}

#[test]
fn let_bindings() {
    assert_same(
        "let_bindings",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       x = 6 * 7\n\
         \x20       y = x - 2\n\
         \x20   in\n\
         \x20   x + y\n",
    );
}

#[test]
fn polymorphic_identity_specialized_to_int() {
    assert_same(
        "poly_identity",
        "module Test exposing (..)\n\
         \n\
         identity : a -> a\n\
         identity x = x\n\
         \n\
         main : Int\n\
         main = identity 7 + identity 35\n",
    );
}
