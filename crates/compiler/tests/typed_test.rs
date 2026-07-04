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
fn case_on_integers() {
    assert_same(
        "case_int",
        "module Test exposing (..)\n\
         \n\
         classify : Int -> Int\n\
         classify n =\n\
         \x20   case n of\n\
         \x20       0 -> 100\n\
         \x20       1 -> 200\n\
         \x20       other -> other * 10\n\
         \n\
         main : Int\n\
         main = classify 0 + classify 1 + classify 5\n",
    );
}

#[test]
fn tuples_construct_and_destructure() {
    assert_same(
        "tuples",
        "module Test exposing (..)\n\
         \n\
         divmod : Int -> Int -> ( Int, Int )\n\
         divmod a b = ( a // b, a - (a // b) * b )\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       ( q, r ) = divmod 17 5\n\
         \x20   in\n\
         \x20   q * 100 + r\n",
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

#[test]
fn records_construct_access_update() {
    assert_same(
        "records",
        "module Test exposing (..)\n\
         \n\
         type alias Point = { x : Int, y : Int }\n\
         \n\
         origin : Point\n\
         origin = { x = 3, y = 4 }\n\
         \n\
         moved : Point\n\
         moved = { origin | x = origin.x + 10 }\n\
         \n\
         main : Int\n\
         main = moved.x * 100 + moved.y\n",
    );
}

#[test]
fn record_field_destructure() {
    assert_same(
        "record_destructure",
        "module Test exposing (..)\n\
         \n\
         type alias Point = { x : Int, y : Int }\n\
         \n\
         sum : Point -> Int\n\
         sum p =\n\
         \x20   let\n\
         \x20       { x, y } = p\n\
         \x20   in\n\
         \x20   x + y\n\
         \n\
         main : Int\n\
         main = sum { x = 40, y = 2 }\n",
    );
}

#[test]
fn custom_enum_and_case() {
    assert_same(
        "enum",
        "module Test exposing (..)\n\
         \n\
         type Color = Red | Green | Blue\n\
         \n\
         toInt : Color -> Int\n\
         toInt c =\n\
         \x20   case c of\n\
         \x20       Red -> 1\n\
         \x20       Green -> 2\n\
         \x20       Blue -> 3\n\
         \n\
         main : Int\n\
         main = toInt Red * 100 + toInt Green * 10 + toInt Blue\n",
    );
}

#[test]
fn bool_case_and_construction() {
    assert_same(
        "bool_case",
        "module Test exposing (..)\n\
         \n\
         isEven : Int -> Bool\n\
         isEven n = n - (n // 2) * 2 == 0\n\
         \n\
         describe : Bool -> Int\n\
         describe b =\n\
         \x20   case b of\n\
         \x20       True -> 1\n\
         \x20       False -> 0\n\
         \n\
         main : Int\n\
         main = describe (isEven 4) * 10 + describe (isEven 7)\n",
    );
}

#[test]
fn maybe_construct_and_match() {
    assert_same(
        "maybe",
        "module Test exposing (..)\n\
         \n\
         safeDiv : Int -> Int -> Maybe Int\n\
         safeDiv a b = if b == 0 then Nothing else Just (a // b)\n\
         \n\
         orZero : Maybe Int -> Int\n\
         orZero m =\n\
         \x20   case m of\n\
         \x20       Just x -> x\n\
         \x20       Nothing -> 0\n\
         \n\
         main : Int\n\
         main = orZero (safeDiv 42 6) * 100 + orZero (safeDiv 1 0)\n",
    );
}

#[test]
fn recursive_tree_sum() {
    assert_same(
        "tree",
        "module Test exposing (..)\n\
         \n\
         type Tree = Leaf Int | Node Tree Tree\n\
         \n\
         sum : Tree -> Int\n\
         sum t =\n\
         \x20   case t of\n\
         \x20       Leaf n -> n\n\
         \x20       Node l r -> sum l + sum r\n\
         \n\
         main : Int\n\
         main = sum (Node (Node (Leaf 3) (Leaf 4)) (Leaf 5))\n",
    );
}

#[test]
fn list_literal_and_recursive_sum() {
    assert_same(
        "list_sum",
        "module Test exposing (..)\n\
         \n\
         sum : List Int -> Int\n\
         sum xs =\n\
         \x20   case xs of\n\
         \x20       [] -> 0\n\
         \x20       h :: t -> h + sum t\n\
         \n\
         main : Int\n\
         main = sum [ 1, 2, 3, 4, 5 ]\n",
    );
}

#[test]
fn list_cons_and_length() {
    assert_same(
        "list_cons",
        "module Test exposing (..)\n\
         \n\
         length : List Int -> Int\n\
         length xs =\n\
         \x20   case xs of\n\
         \x20       [] -> 0\n\
         \x20       _ :: t -> 1 + length t\n\
         \n\
         main : Int\n\
         main = length (1 :: 2 :: 3 :: [])\n",
    );
}

#[test]
fn generated_list_kernels() {
    assert_same(
        "list_kernels",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   List.sum (List.range 1 100) + List.length (List.range 1 50)\n",
    );
}

#[test]
fn higher_order_kernels_with_named_functions() {
    assert_same(
        "hof_kernels",
        "module Test exposing (..)\n\
         \n\
         square : Int -> Int\n\
         square n = n * n\n\
         \n\
         add : Int -> Int -> Int\n\
         add x acc = x + acc\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   List.range 1 10\n\
         \x20       |> List.map square\n\
         \x20       |> List.foldl add 0\n",
    );
}

#[test]
fn deep_tail_recursion() {
    // 1,000,000-deep self-recursion in tail position. If LLVM's tail-call
    // elimination turns it into a loop we're fine; otherwise this overflows
    // the stack (signalling the typed backend needs its own tail-loop).
    assert_same(
        "deep_tail",
        "module Test exposing (..)\n\
         \n\
         sum : Int -> Int -> Int\n\
         sum n acc = if n <= 0 then acc else sum (n - 1) (acc + n)\n\
         \n\
         main : Int\n\
         main = sum 1000000 0\n",
    );
}
