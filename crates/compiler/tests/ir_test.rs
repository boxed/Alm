//! Tests for the lowered IR (`ir::lower`): closure conversion, call
//! resolution, pattern compilation, and tail-call marking.

use alm_compiler::{canonicalize, ir, parse};

fn lower(source: &str) -> String {
    let module = parse::parse_module(source)
        .unwrap_or_else(|e| panic!("parse failed: {}", e.message));
    let canonical = canonicalize::canonicalize(&module)
        .unwrap_or_else(|es| panic!("canonicalize failed: {}", es[0].message));
    let pretty = ir::lower::lower_project(std::slice::from_ref(&canonical)).to_pretty();
    println!("{}", pretty);
    pretty
}

#[test]
fn saturated_call_is_direct() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         add x y = x + y\n\
         \n\
         main = add 1 2\n",
    );
    assert!(ir.contains("function $Test$add(x, y):"));
    assert!(ir.contains("(add x y)"));
    assert!(ir.contains("(call $Test$add 1 2)"));
    assert!(ir.contains("main = $Test$main"));
}

#[test]
fn partial_application_goes_through_apply() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         add x y = x + y\n\
         \n\
         inc = add 1\n",
    );
    assert!(ir.contains("(apply (closure $Test$add) 1)"));
}

#[test]
fn lambda_is_lifted_with_captures() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         makeAdder n = \\x -> x + n\n",
    );
    // The lambda becomes a top-level function whose first parameter is
    // the captured `n` (printed with a ^ marker).
    assert!(ir.contains("(^n, x):"));
    assert!(ir.contains("(add x n)"));
    assert!(ir.contains("[n])"), "closure should capture n: {}", ir);
}

#[test]
fn self_tail_call_becomes_loop() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         count n acc = if n <= 0 then acc else count (n - 1) (acc + n)\n",
    );
    assert!(ir.contains("function (tail) $Test$count(n, acc):"));
    assert!(ir.contains("(tail-call (sub n 1) (add acc n))"));
}

#[test]
fn list_patterns_compile_to_tests() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         describe xs =\n\
         \x20   case xs of\n\
         \x20       [] -> \"empty\"\n\
         \x20       x :: _ -> x\n",
    );
    assert!(ir.contains("is-nil"));
    assert!(ir.contains("is-cons"));
    assert!(ir.contains("head)]"), "cons branch should bind the head: {}", ir);
}

#[test]
fn custom_types_construct_and_match_by_index() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         type Shape = Circle Float | Square Float\n\
         \n\
         area s =\n\
         \x20   case s of\n\
         \x20       Circle r -> r\n\
         \x20       Square w -> w\n\
         \n\
         main = area (Circle 1.5)\n",
    );
    assert!(ir.contains("(ctor Circle #0 1.5f)"));
    assert!(ir.contains("is-ctor[Circle #0]"));
    assert!(ir.contains("is-ctor[Square #1]"));
    assert!(ir.contains("arg0)]"), "branches should bind the payload: {}", ir);
}

#[test]
fn unsaturated_constructor_becomes_wrapper_closure() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         type Shape = Circle Float | Square Float\n\
         \n\
         circles = List.map Circle [1.0, 2.0]\n",
    );
    assert!(ir.contains("function $Test$Circle(a0):"));
    assert!(ir.contains("(closure $Test$Circle)"));
    // A saturated call to a kernel builtin lowers to a direct call.
    assert!(ir.contains("(builtin rtb$List$map (closure $Test$Circle)"));
}

#[test]
fn local_recursive_function_is_letrec_with_tail_loop() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         len xs =\n\
         \x20   let\n\
         \x20       go acc ys =\n\
         \x20           case ys of\n\
         \x20               [] -> acc\n\
         \x20               _ :: rest -> go (acc + 1) rest\n\
         \x20   in\n\
         \x20   go 0 xs\n",
    );
    assert!(ir.contains("letrec"));
    assert!(ir.contains("function (tail)"));
    assert!(ir.contains("(tail-call (add acc 1) rest)"));
}

#[test]
fn accessors_and_records() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         getName = .name\n\
         \n\
         main = getName { name = \"alm\" }\n",
    );
    assert!(ir.contains("function $accessor$name(r):"));
    assert!(ir.contains("(access r name)"));
    assert!(ir.contains("(record (name \"alm\"))"));
}

#[test]
fn booleans_lower_to_primitives() {
    let ir = lower(
        "module Test exposing (..)\n\
         \n\
         pick b = if b then 1 else 2\n\
         \n\
         flag = True\n",
    );
    assert!(ir.contains("if b then"));
    assert!(ir.contains("true"));
}
