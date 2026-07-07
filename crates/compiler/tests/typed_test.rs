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
fn strings_and_fromint() {
    assert_same(
        "strings",
        "module Test exposing (..)\n\
         \n\
         greet : Int -> String\n\
         greet n = \"tick \" ++ String.fromInt n\n\
         \n\
         main : String\n\
         main = greet 42 ++ \"!\"\n",
    );
}

#[test]
fn lambdas_in_kernels() {
    assert_same(
        "lambdas",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       k = 3\n\
         \x20   in\n\
         \x20   List.range 1 10\n\
         \x20       |> List.map (\\x -> x * k)\n\
         \x20       |> List.foldl (\\x acc -> x + acc) 0\n",
    );
}

#[test]
fn filter_reverse_pipeline() {
    assert_same(
        "filter_reverse",
        "module Test exposing (..)\n\
         \n\
         isEven : Int -> Bool\n\
         isEven n = n - (n // 2) * 2 == 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   List.range 1 20\n\
         \x20       |> List.filter isEven\n\
         \x20       |> List.reverse\n\
         \x20       |> List.sum\n",
    );
}

#[test]
fn basics_numeric_kernels() {
    assert_same(
        "basics",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   modBy 7 100 + remainderBy 7 -100 + abs -42\n",
    );
}

#[test]
fn to_float_kernel() {
    assert_same(
        "to_float",
        "module Test exposing (..)\n\
         \n\
         main : Float\n\
         main = toFloat 10 * 1.5 + toFloat (abs -3)\n",
    );
}

#[test]
fn short_circuit_boolean_ops() {
    assert_same(
        "shortcircuit",
        "module Test exposing (..)\n\
         \n\
         safe : Int -> Bool\n\
         safe x = x /= 0 && 10 // x > 0\n\
         \n\
         describe : Bool -> Int\n\
         describe b = if b then 1 else 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   describe (safe 0)\n\
         \x20       + 10 * describe (safe 5)\n\
         \x20       + 100 * describe (True || safe 0)\n",
    );
}

#[test]
fn foldr_and_minmax() {
    assert_same(
        "foldr_minmax",
        "module Test exposing (..)\n\
         \n\
         diffs : Int\n\
         diffs = List.foldr (\\x acc -> x - acc) 0 [ 1, 2, 3, 4 ]\n\
         \n\
         main : Int\n\
         main = diffs + min 3 9 + max 3 9\n",
    );
}

#[test]
fn let_bound_number_defaults_to_int() {
    // `total` is generalized by the checker to a `number` scheme; the
    // unresolved number must default to Int (as Elm does) so the list and
    // arithmetic inside get an Int layout.
    assert_same(
        "let_number",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       total = List.sum [ 1, 2, 3, 4 ] + 10\n\
         \x20   in\n\
         \x20   total * 2\n",
    );
}

#[test]
fn float_to_int_kernels() {
    assert_same(
        "float_kernels",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   round 2.5\n\
         \x20       + floor 2.9\n\
         \x20       + ceiling 2.1\n\
         \x20       + truncate -2.9\n\
         \x20       + round (sqrt 16.0)\n",
    );
}

#[test]
fn string_length_and_join() {
    assert_same(
        "string_join",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       joined = String.join \", \" [ \"a\", \"bb\", \"ccc\" ]\n\
         \x20   in\n\
         \x20   joined ++ \" len=\" ++ String.fromInt (String.length joined)\n",
    );
}

#[test]
fn kitchen_sink_composition() {
    // Records in a list, filter+map with lambdas doing field access, a named
    // function passed to map, foldl accumulation, string building and join —
    // all composed into one String result.
    assert_same(
        "kitchen_sink",
        "module Test exposing (..)\n\
         \n\
         type alias Item = { name : String, qty : Int }\n\
         \n\
         total : List Item -> Int\n\
         total items = List.foldl (\\i acc -> acc + i.qty) 0 items\n\
         \n\
         describe : Item -> String\n\
         describe i = i.name ++ \":\" ++ String.fromInt i.qty\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       items = [ { name = \"a\", qty = 3 }, { name = \"b\", qty = 5 }, { name = \"c\", qty = 2 } ]\n\
         \x20       big = List.filter (\\i -> i.qty > 2) items\n\
         \x20   in\n\
         \x20   String.join \", \" (List.map describe big)\n\
         \x20       ++ \" total=\"\n\
         \x20       ++ String.fromInt (total items)\n",
    );
}

#[test]
fn list_append_and_member() {
    assert_same(
        "list_append",
        "module Test exposing (..)\n\
         \n\
         boolInt : Bool -> Int\n\
         boolInt b = if b then 1 else 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       joined = [ 1, 2, 3 ] ++ [ 4, 5 ]\n\
         \x20   in\n\
         \x20   List.sum joined * 100\n\
         \x20       + boolInt (List.member 4 joined) * 10\n\
         \x20       + boolInt (List.member 9 joined)\n",
    );
}

#[test]
fn list_head_tail_maybe() {
    assert_same(
        "head_tail",
        "module Test exposing (..)\n\
         \n\
         firstOr : Int -> List Int -> Int\n\
         firstOr default xs =\n\
         \x20   case List.head xs of\n\
         \x20       Just x -> x\n\
         \x20       Nothing -> default\n\
         \n\
         sumTail : List Int -> Int\n\
         sumTail xs =\n\
         \x20   case List.tail xs of\n\
         \x20       Just rest -> List.sum rest\n\
         \x20       Nothing -> -1\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   firstOr 0 [ 7, 8, 9 ]\n\
         \x20       + 100 * firstOr 0 []\n\
         \x20       + 1000 * sumTail [ 7, 8, 9 ]\n",
    );
}

#[test]
fn map2_and_indexed_map() {
    assert_same(
        "map2_indexed",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       dots = List.map2 (\\a b -> a * b) [ 1, 2, 3 ] [ 10, 20, 30, 40 ]\n\
         \x20       weighted = List.indexedMap (\\i x -> i * x) [ 5, 6, 7 ]\n\
         \x20   in\n\
         \x20   List.sum dots * 1000 + List.sum weighted\n",
    );
}

#[test]
fn string_equality_compares_contents() {
    // Regression: == on strings must compare contents, not pointer words —
    // two separately-built equal strings must be equal.
    assert_same(
        "string_eq",
        "module Test exposing (..)\n\
         \n\
         boolInt : Bool -> Int\n\
         boolInt b = if b then 1 else 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   boolInt ((\"a\" ++ \"b\") == \"ab\")\n\
         \x20       + 10 * boolInt (\"ab\" == \"ba\")\n\
         \x20       + 100 * boolInt (\"abc\" < \"abd\")\n",
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

#[test]
fn clamp_kernel() {
    assert_same(
        "clamp",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main = clamp 0 10 -5 + clamp 0 10 15 * 10 + clamp 0 10 7 * 100\n",
    );
}

#[test]
fn first_class_closures() {
    // A lambda bound to a variable and applied; a user function taking a
    // function parameter; a named function passed as a value; a captured
    // free variable.
    assert_same(
        "closures",
        "module Test exposing (..)\n\
         \n\
         applyTwice : (Int -> Int) -> Int -> Int\n\
         applyTwice f x = f (f x)\n\
         \n\
         increment : Int -> Int\n\
         increment n = n + 1\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       k = 10\n\
         \x20       addK = \\x -> x + k\n\
         \x20   in\n\
         \x20   applyTwice addK 5\n\
         \x20       + 100 * applyTwice increment 5\n\
         \x20       + 10000 * addK 0\n",
    );
}

#[test]
fn closure_returned_from_function() {
    // A function that returns a closure capturing its argument.
    assert_same(
        "closure_return",
        "module Test exposing (..)\n\
         \n\
         adder : Int -> (Int -> Int)\n\
         adder n = \\x -> x + n\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       add5 = adder 5\n\
         \x20   in\n\
         \x20   add5 100 + add5 200\n",
    );
}

#[test]
fn closure_argument_into_kernel() {
    // A function-typed parameter passed through to List.map — the kernel must
    // apply the closure value, not require a named function or lambda.
    assert_same(
        "closure_kernel",
        "module Test exposing (..)\n\
         \n\
         mapAll : (Int -> Int) -> List Int -> List Int\n\
         mapAll f xs = List.map f xs\n\
         \n\
         main : Int\n\
         main = List.sum (mapAll (\\x -> x * 2) [ 1, 2, 3, 4 ])\n",
    );
}

#[test]
fn structural_equality_tuples_records() {
    assert_same(
        "struct_eq",
        "module Test exposing (..)\n\
         \n\
         type alias P = { x : Int, y : Int }\n\
         \n\
         boolInt : Bool -> Int\n\
         boolInt b = if b then 1 else 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   boolInt (( 1, 2 ) == ( 1, 2 ))\n\
         \x20       + 10 * boolInt (( 1, 2 ) == ( 1, 3 ))\n\
         \x20       + 100 * boolInt ({ x = 1, y = 2 } == { x = 1, y = 2 })\n\
         \x20       + 1000 * boolInt ({ x = 1, y = 2 } /= { x = 9, y = 2 })\n",
    );
}

#[test]
fn structural_equality_lists() {
    assert_same(
        "list_eq",
        "module Test exposing (..)\n\
         \n\
         boolInt : Bool -> Int\n\
         boolInt b = if b then 1 else 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   boolInt ([ 1, 2, 3 ] == [ 1, 2, 3 ])\n\
         \x20       + 10 * boolInt ([ 1, 2 ] == [ 1, 2, 3 ])\n\
         \x20       + 100 * boolInt ([ 1, 2, 3 ] == [ 1, 9, 3 ])\n\
         \x20       + 1000 * boolInt ([] == [])\n",
    );
}

#[test]
fn structural_equality_unions() {
    assert_same(
        "union_eq",
        "module Test exposing (..)\n\
         \n\
         boolInt : Bool -> Int\n\
         boolInt b = if b then 1 else 0\n\
         \n\
         mk : Int -> Maybe Int\n\
         mk n = if n < 0 then Nothing else Just n\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   boolInt (mk 5 == Just 5)\n\
         \x20       + 10 * boolInt (mk 5 == Just 6)\n\
         \x20       + 100 * boolInt (mk -1 == Nothing)\n\
         \x20       + 1000 * boolInt (mk 5 == Nothing)\n",
    );
}

#[test]
fn partial_application() {
    // A named function applied to fewer args than its arity becomes a
    // closure; passing that to a kernel and applying it directly both work.
    assert_same(
        "partial",
        "module Test exposing (..)\n\
         \n\
         add : Int -> Int -> Int\n\
         add a b = a + b\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       add10 = add 10\n\
         \x20   in\n\
         \x20   add10 5 + List.sum (List.map (add 100) [ 1, 2, 3 ])\n",
    );
}

#[test]
fn function_composition() {
    assert_same(
        "compose",
        "module Test exposing (..)\n\
         \n\
         inc : Int -> Int\n\
         inc n = n + 1\n\
         \n\
         double : Int -> Int\n\
         double n = n * 2\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       f = inc << double\n\
         \x20       g = inc >> double\n\
         \x20   in\n\
         \x20   f 10 + 100 * g 10\n",
    );
}

#[test]
fn record_accessor_as_function() {
    // `.field` used as a first-class function (passed to List.map).
    assert_same(
        "accessor",
        "module Test exposing (..)\n\
         \n\
         type alias P = { x : Int, y : Int }\n\
         \n\
         points : List P\n\
         points = [ { x = 1, y = 10 }, { x = 2, y = 20 }, { x = 3, y = 30 } ]\n\
         \n\
         main : Int\n\
         main = List.sum (List.map .x points) * 100 + List.sum (List.map .y points)\n",
    );
}

#[test]
fn take_drop_all_any() {
    assert_same(
        "take_drop",
        "module Test exposing (..)\n\
         \n\
         boolInt : Bool -> Int\n\
         boolInt b = if b then 1 else 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       xs = List.range 1 10\n\
         \x20   in\n\
         \x20   List.sum (List.take 3 xs) * 1000\n\
         \x20       + List.sum (List.drop 7 xs) * 10\n\
         \x20       + boolInt (List.all (\\n -> n > 0) xs)\n\
         \x20       + 2 * boolInt (List.any (\\n -> n > 100) xs)\n",
    );
}

#[test]
fn cross_module_specialization() {
    // A helper module imported by the entry: the polymorphic helper must be
    // specialized in its own module and called across the boundary.
    let dir = std::env::temp_dir()
        .join("alm-typed-xmod")
        .join(format!("xmod-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create dir");
    std::fs::write(
        dir.join("Helper.elm"),
        "module Helper exposing (..)\n\
         \n\
         twice : (a -> a) -> a -> a\n\
         twice f x = f (f x)\n\
         \n\
         bump : Int -> Int\n\
         bump n = n + 1\n",
    )
    .unwrap();
    let entry = dir.join("Main.elm");
    std::fs::write(
        &entry,
        "module Main exposing (..)\n\
         \n\
         import Helper\n\
         \n\
         main : Int\n\
         main = Helper.twice Helper.bump 40\n",
    )
    .unwrap();

    // JS backend.
    let js = alm_compiler::project::compile_project(&entry)
        .unwrap_or_else(|errs| panic!("js compile failed with {} errors", errs.len()));
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, js).unwrap();
    let js_out = run(Command::new("node").arg("-e").arg(format!(
        "console.log(require({:?})['Main']['main'])",
        bundle.display()
    )));

    // Typed native backend.
    let binary = dir.join("main");
    alm_compiler::project::compile_project_typed(&entry, &binary, Target::Native)
        .unwrap_or_else(|errs| {
            panic!(
                "typed compile failed:\n{}",
                errs.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
            )
        });
    let native_out = run(&mut Command::new(&binary));

    assert_eq!(native_out, js_out, "cross-module typed vs JS");
}

#[test]
fn debug_tostring_values() {
    assert_same(
        "debug",
        "module Test exposing (..)\n\
         \n\
         type alias P = { x : Int, y : Int }\n\
         \n\
         main : String\n\
         main =\n\
         \x20   Debug.toString ( 1, 2.5, True )\n\
         \x20       ++ \" \" ++ Debug.toString [ 1, 2, 3 ]\n\
         \x20       ++ \" \" ++ Debug.toString { x = 7, y = 8 }\n\
         \x20       ++ \" \" ++ Debug.toString \"hi\"\n",
    );
}

#[test]
fn debug_tostring_unions() {
    assert_same(
        "debug_unions",
        "module Test exposing (..)\n\
         \n\
         type Shape = Circle Int | Rect Int Int | Dot\n\
         \n\
         main : String\n\
         main =\n\
         \x20   Debug.toString (Circle 5)\n\
         \x20       ++ \" \" ++ Debug.toString (Rect 3 4)\n\
         \x20       ++ \" \" ++ Debug.toString Dot\n\
         \x20       ++ \" \" ++ Debug.toString (Just [ 1, 2 ])\n\
         \x20       ++ \" \" ++ Debug.toString Nothing\n",
    );
}

#[test]
fn string_toint_tofloat_unbox() {
    // String.toInt/toFloat return Maybe from the runtime, unboxed back to the
    // typed representation and matched.
    assert_same(
        "toint",
        "module Test exposing (..)\n\
         \n\
         parse : String -> Int\n\
         parse s =\n\
         \x20   case String.toInt s of\n\
         \x20       Just n -> n\n\
         \x20       Nothing -> -1\n\
         \n\
         main : String\n\
         main =\n\
         \x20   String.fromInt (parse \"42\")\n\
         \x20       ++ \" \" ++ String.fromInt (parse \"oops\")\n\
         \x20       ++ \" \" ++ Debug.toString (String.toFloat \"3.5\")\n",
    );
}

#[test]
fn tea_worker_ticks() {
    // A Platform.worker program with a timer subscription and Terminal
    // output, compiled through the typed backend and driven by the runtime's
    // TEA loop.
    let dir = std::env::temp_dir()
        .join("alm-typed-tea")
        .join(format!("tea-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (..)\n\
         \n\
         import Time\n\
         \n\
         type Msg = Tick Time.Posix\n\
         \n\
         main =\n\
         \x20   Platform.worker { init = \\_ -> ( 0, Cmd.none ), update = update, subscriptions = subs }\n\
         \n\
         update msg model =\n\
         \x20   case msg of\n\
         \x20       Tick _ -> ( model + 1, Terminal.writeLine (\"tick \" ++ String.fromInt (model + 1)) )\n\
         \n\
         subs model =\n\
         \x20   if model < 3 then Time.every 10 Tick else Sub.none\n",
    )
    .unwrap();

    let binary = dir.join("test");
    alm_compiler::project::compile_project_typed(&entry, &binary, Target::Native)
        .unwrap_or_else(|errs| {
            panic!(
                "typed compile failed:\n{}",
                errs.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
            )
        });
    let out = run(&mut Command::new(&binary));
    assert_eq!(out, "tick 1\ntick 2\ntick 3");
}

#[test]
fn tea_task_chain() {
    // Task.succeed/andThen/map piped through Task.perform, delivered as a Msg
    // and printed — the task interpreter on the typed backend.
    let dir = std::env::temp_dir()
        .join("alm-typed-task")
        .join(format!("task-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (..)\n\
         \n\
         import Task\n\
         \n\
         type Msg = Done Int\n\
         \n\
         main =\n\
         \x20   Platform.worker { init = init, update = update, subscriptions = \\_ -> Sub.none }\n\
         \n\
         init _ =\n\
         \x20   ( 0\n\
         \x20   , Task.succeed 20\n\
         \x20       |> Task.andThen (\\n -> Task.succeed (n + 1))\n\
         \x20       |> Task.map (\\n -> n * 2)\n\
         \x20       |> Task.perform Done\n\
         \x20   )\n\
         \n\
         update msg model =\n\
         \x20   case msg of\n\
         \x20       Done n -> ( model, Terminal.writeLine (String.fromInt n) )\n",
    )
    .unwrap();
    let binary = dir.join("test");
    alm_compiler::project::compile_project_typed(&entry, &binary, Target::Native)
        .unwrap_or_else(|errs| {
            panic!(
                "typed compile failed:\n{}",
                errs.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
            )
        });
    let out = run(&mut Command::new(&binary));
    assert_eq!(out, "42");
}

#[test]
fn dict_operations() {
    assert_same(
        "dict",
        "module Test exposing (..)\n\
         \n\
         import Dict\n\
         \n\
         d : Dict.Dict Int String\n\
         d = Dict.fromList [ ( 3, \"c\" ), ( 1, \"a\" ), ( 2, \"b\" ) ]\n\
         \n\
         orDash : Maybe String -> String\n\
         orDash m = case m of\n\
         \x20   Just s -> s\n\
         \x20   Nothing -> \"-\"\n\
         \n\
         main : String\n\
         main =\n\
         \x20   String.fromInt (Dict.size d)\n\
         \x20       ++ \" \" ++ orDash (Dict.get 2 d)\n\
         \x20       ++ \" \" ++ orDash (Dict.get 9 d)\n\
         \x20       ++ \" \" ++ String.join \",\" (Dict.values d)\n\
         \x20       ++ \" \" ++ Debug.toString (Dict.keys d)\n\
         \x20       ++ \" \" ++ String.fromInt (Dict.foldl (\\_ _ n -> n + 1) 0 (Dict.insert 5 \"e\" d))\n",
    );
}

#[test]
fn dict_merge() {
    assert_same(
        "dict_merge",
        "module Test exposing (..)\n\
         \n\
         import Dict\n\
         \n\
         a : Dict.Dict Int String\n\
         a = Dict.fromList [ ( 1, \"a1\" ), ( 2, \"a2\" ), ( 4, \"a4\" ) ]\n\
         \n\
         b : Dict.Dict Int String\n\
         b = Dict.fromList [ ( 2, \"b2\" ), ( 3, \"b3\" ), ( 4, \"b4\" ) ]\n\
         \n\
         left : Int -> String -> List String -> List String\n\
         left k v acc = acc ++ [ String.fromInt k ++ \"L\" ++ v ]\n\
         \n\
         both : Int -> String -> String -> List String -> List String\n\
         both k v1 v2 acc = acc ++ [ String.fromInt k ++ \"B\" ++ v1 ++ v2 ]\n\
         \n\
         right : Int -> String -> List String -> List String\n\
         right k v acc = acc ++ [ String.fromInt k ++ \"R\" ++ v ]\n\
         \n\
         main : String\n\
         main =\n\
         \x20   String.join \",\" (Dict.merge left both right a b [])\n",
    );
}

#[test]
fn set_operations() {
    assert_same(
        "set",
        "module Test exposing (..)\n\
         \n\
         import Set\n\
         \n\
         boolInt : Bool -> Int\n\
         boolInt b = if b then 1 else 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       s = Set.fromList [ 3, 1, 2, 1, 3 ]\n\
         \x20   in\n\
         \x20   Set.size s * 100\n\
         \x20       + boolInt (Set.member 2 s) * 10\n\
         \x20       + boolInt (Set.member 9 s)\n\
         \x20       + 1000 * List.sum (Set.toList (Set.union s (Set.fromList [ 4, 2 ])))\n",
    );
}

#[test]
fn array_operations() {
    assert_same(
        "array",
        "module Test exposing (..)\n\
         \n\
         import Array\n\
         \n\
         orZero : Maybe Int -> Int\n\
         orZero m = case m of\n\
         \x20   Just n -> n\n\
         \x20   Nothing -> 0\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let\n\
         \x20       a = Array.push 99 (Array.fromList [ 10, 20, 30 ])\n\
         \x20   in\n\
         \x20   Array.length a * 10000\n\
         \x20       + orZero (Array.get 1 a) * 100\n\
         \x20       + orZero (Array.get 3 a)\n\
         \x20       + Array.foldl (\\x acc -> acc + x) 0 (Array.map (\\x -> x * 2) a)\n",
    );
}

#[test]
fn bitwise_and_trig_via_generic_foreign() {
    assert_same(
        "generic_foreign",
        "module Test exposing (..)\n\
         \n\
         import Bitwise\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   Bitwise.and 12 10\n\
         \x20       + Bitwise.or 12 10 * 100\n\
         \x20       + Bitwise.shiftLeftBy 4 1 * 10000\n\
         \x20       + round (cos 0.0) * 1000000\n",
    );
}

#[test]
fn builtin_as_first_class_value() {
    // A kernel function passed as a value (not applied) must be eta-expanded
    // into a closure `List.map` can call, not routed into the saturated
    // kernel-call path with no arguments.
    assert_same(
        "builtin_value",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main = String.join \",\" (List.map String.fromInt [ 1, 2, 3 ])\n",
    );
}

#[test]
fn builtin_partial_application() {
    // A kernel applied to fewer than its arity of arguments (`modBy 3`) must
    // eta-expand into a closure taking the remaining argument.
    assert_same(
        "builtin_partial",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   String.join \",\"\n\
         \x20       (List.map String.fromInt (List.map (modBy 3) [ 1, 4, 5, 7 ]))\n",
    );
}

#[test]
fn debug_recursive_union() {
    // Debug.toString of a recursive union must terminate at codegen (the box
    // helper follows the recursive Ref field via a real recursive call).
    assert_same(
        "debug_recursive",
        "module Test exposing (..)\n\
         \n\
         type Tree = Leaf Int | Node Tree Tree\n\
         \n\
         main : String\n\
         main = Debug.toString (Node (Node (Leaf 1) (Leaf 2)) (Leaf 3))\n",
    );
}

#[test]
fn debug_tuple_with_recursive_union() {
    // A tuple containing a recursive union boxes each component correctly.
    assert_same(
        "debug_tuple_recursive",
        "module Test exposing (..)\n\
         \n\
         type Tree = Leaf Int | Node Tree Tree\n\
         \n\
         sum : Tree -> Int\n\
         sum t =\n\
         \x20   case t of\n\
         \x20       Leaf n -> n\n\
         \x20       Node a b -> sum a + sum b\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       t = Node (Leaf 1) (Leaf 2)\n\
         \x20   in\n\
         \x20   Debug.toString ( sum t, Just t )\n",
    );
}

#[test]
fn lambda_unit_param() {
    // `\() -> e` thunks: destructuring a unit parameter.
    assert_same(
        "lambda_unit_param",
        "module Test exposing (..)\n\
         \n\
         apply : List (() -> Int) -> Int\n\
         apply fs = List.sum (List.map (\\f -> f ()) fs)\n\
         \n\
         main : String\n\
         main = String.fromInt (apply [ \\() -> 1, \\() -> 2, \\() -> 3 ])\n",
    );
}

#[test]
fn lambda_constructor_param() {
    // `\(Id n) -> n`: destructuring a single-variant constructor parameter.
    assert_same(
        "lambda_ctor_param",
        "module Test exposing (..)\n\
         \n\
         type Id = Id Int\n\
         \n\
         main : String\n\
         main = String.fromInt (List.sum (List.map (\\(Id n) -> n) [ Id 1, Id 2, Id 3 ]))\n",
    );
}

#[test]
fn lambda_tuple_param() {
    // `\(a, b) -> a + b`: destructuring a tuple parameter.
    assert_same(
        "lambda_tuple_param",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main = String.fromInt ((\\( a, b ) -> a + b) ( 3, 4 ))\n",
    );
}

#[test]
fn lambda_record_param() {
    // `\{x} -> x`: destructuring a record parameter.
    assert_same(
        "lambda_record_param",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main = String.fromInt ((\\{ x } -> x) { x = 5 })\n",
    );
}

#[test]
fn lambda_mixed_pattern_params() {
    // Combined repro: constructor + tuple + record lambda params in one expr.
    assert_same(
        "lambda_mixed_params",
        "module Test exposing (..)\n\
         \n\
         type Id = Id Int\n\
         \n\
         main : String\n\
         main =\n\
         \x20   String.fromInt\n\
         \x20       (List.sum (List.map (\\(Id n) -> n) [ Id 1, Id 2 ])\n\
         \x20           + (\\( a, b ) -> a + b) ( 3, 4 )\n\
         \x20           + (\\{ x } -> x) { x = 5 }\n\
         \x20       )\n",
    );
}

#[test]
fn unbox_closure_from_dict() {
    // A function value stored in a Dict flows through the boxed/uniform slot;
    // retrieving it must unbox the uniform closure into a typed closure that
    // can be called with the typed calling convention.
    assert_same(
        "unbox_closure_dict",
        "module Test exposing (..)\n\
         \n\
         import Dict\n\
         \n\
         main : String\n\
         main =\n\
         \x20   case Dict.get \"a\" (Dict.fromList [ ( \"a\", \\n -> n + 1 ), ( \"b\", \\n -> n * 2 ) ]) of\n\
         \x20       Just f -> String.fromInt (f 10)\n\
         \x20       Nothing -> \"none\"\n",
    );
}

#[test]
fn unbox_curried_closure_from_dict() {
    // A curried (multi-argument) function unboxed from a Dict value.
    assert_same(
        "unbox_curried_closure_dict",
        "module Test exposing (..)\n\
         \n\
         import Dict\n\
         \n\
         main : String\n\
         main =\n\
         \x20   case Dict.get \"add\" (Dict.fromList [ ( \"add\", \\a b -> a + b ) ]) of\n\
         \x20       Just f -> String.fromInt (f 10 32)\n\
         \x20       Nothing -> \"none\"\n",
    );
}

#[test]
fn local_function_definition() {
    // A non-recursive `let`-bound function, closure-converted like a lambda.
    assert_same(
        "local_fn",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       inc x = x + 1\n\
         \x20   in\n\
         \x20   String.fromInt (inc 41)\n",
    );
}

#[test]
fn local_function_captures_outer() {
    // A local function that captures an enclosing `let` binding.
    assert_same(
        "local_fn_capture",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       base = 100\n\
         \x20       add n = base + n\n\
         \x20   in\n\
         \x20   String.fromInt (add 23)\n",
    );
}

#[test]
fn recursive_local_function() {
    // A self-recursive `let`-bound function reaches itself via its environment.
    assert_same(
        "rec_local_fn",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       go n acc = if n == 0 then acc else go (n - 1) (acc + n)\n\
         \x20   in\n\
         \x20   String.fromInt (go 100 0)\n",
    );
}

#[test]
fn local_function_pattern_param() {
    // A `let`-bound function with a destructuring (constructor) parameter.
    assert_same(
        "local_fn_pat",
        "module Test exposing (..)\n\
         \n\
         type Id = Id Int\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       unwrap (Id n) = n\n\
         \x20   in\n\
         \x20   String.fromInt (unwrap (Id 7))\n",
    );
}

#[test]
fn toplevel_pattern_param() {
    // A top-level function whose parameter is a destructuring pattern.
    assert_same(
        "toplevel_pat_param",
        "module Test exposing (..)\n\
         \n\
         type Id = Id Int\n\
         \n\
         unwrap : Id -> Int\n\
         unwrap (Id n) = n\n\
         \n\
         main : String\n\
         main = String.fromInt (unwrap (Id 42))\n",
    );
}

#[test]
fn partial_app_in_kernel() {
    // A multi-parameter lambda applied to fewer arguments by a kernel
    // (`List.map` applies a 3-param lambda to one element) yields a partial
    // closure, later saturated.
    assert_same(
        "partial_app_kernel",
        "module Test exposing (..)\n\
         \n\
         main : Int\n\
         main =\n\
         \x20   let fs = List.map (\\a b c -> a + b + c) [ 1, 2, 3 ]\n\
         \x20   in List.foldl (\\g acc -> acc + g 10 20) 0 fs\n",
    );
}

#[test]
fn mutually_recursive_local_functions() {
    assert_same(
        "mutual_rec",
        "module Test exposing (..)\n\
         \n\
         classify : Int -> String\n\
         classify n =\n\
         \x20   let\n\
         \x20       isEven x = if x == 0 then True else isOdd (x - 1)\n\
         \x20       isOdd x = if x == 0 then False else isEven (x - 1)\n\
         \x20   in\n\
         \x20   if isEven n then \"even\" else \"odd\"\n\
         \n\
         main : String\n\
         main = classify 10 ++ \",\" ++ classify 7\n",
    );
}

#[test]
fn nested_refutable_tuple_pattern() {
    assert_same(
        "nested_tuple_pat",
        "module Test exposing (..)\n\
         \n\
         describe : ( Maybe Int, Int ) -> String\n\
         describe p =\n\
         \x20   case p of\n\
         \x20       ( Just x, 0 ) -> \"jz\" ++ String.fromInt x\n\
         \x20       ( Just x, _ ) -> \"j\" ++ String.fromInt x\n\
         \x20       ( Nothing, n ) -> \"n\" ++ String.fromInt n\n\
         \n\
         main : String\n\
         main = describe ( Just 5, 0 ) ++ describe ( Just 7, 9 ) ++ describe ( Nothing, 3 )\n",
    );
}

#[test]
fn nonempty_list_and_cons_alias_patterns() {
    assert_same(
        "list_cons_pat",
        "module Test exposing (..)\n\
         \n\
         scanr1 : List Int -> List Int\n\
         scanr1 xs_ =\n\
         \x20   case xs_ of\n\
         \x20       [] -> []\n\
         \x20       [ x ] -> [ x * 100 ]\n\
         \x20       x :: xs ->\n\
         \x20           case scanr1 xs of\n\
         \x20               (q :: _) as qs -> (x + q) :: qs\n\
         \x20               [] -> []\n\
         \n\
         main : String\n\
         main = String.join \",\" (List.map String.fromInt (scanr1 [ 1, 2, 3 ]))\n",
    );
}

#[test]
fn char_literal_value() {
    assert_same(
        "char_lit",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main = String.fromChar 'Z'\n",
    );
}

#[test]
fn min_max_on_strings() {
    assert_same(
        "min_max_str",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main = min \"banana\" \"apple\" ++ \"|\" ++ max \"banana\" \"apple\"\n",
    );
}

#[test]
fn exponent_operator() {
    assert_same(
        "exponent",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   String.fromInt (2 ^ 10) ++ \"|\" ++ String.fromFloat (2.0 ^ 0.5)\n",
    );
}

#[test]
fn list_member_strings() {
    assert_same(
        "list_member_str",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let ok = List.member \"b\" [ \"a\", \"b\", \"c\" ]\n\
         \x20       no = List.member \"z\" [ \"a\", \"b\", \"c\" ]\n\
         \x20   in (if ok then \"y\" else \"n\") ++ (if no then \"y\" else \"n\")\n",
    );
}

#[test]
fn constructor_let_destructure() {
    assert_same(
        "ctor_destructure",
        "module Test exposing (..)\n\
         \n\
         type Wrapper = Wrapper Int String\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let (Wrapper n s) = Wrapper 42 \"hi\"\n\
         \x20   in s ++ String.fromInt n\n",
    );
}

#[test]
fn row_polymorphic_record_field() {
    // A helper with no annotation is row-polymorphic in the record; the
    // concrete record must still lay out with all its fields.
    assert_same(
        "row_poly_record",
        "module Test exposing (..)\n\
         \n\
         type alias Runner = { labels : List String, run : () -> Int }\n\
         \n\
         runners : List Runner\n\
         runners =\n\
         \x20   [ { labels = [ \"a\" ], run = \\() -> 10 }\n\
         \x20   , { labels = [ \"b\", \"c\" ], run = \\() -> 20 }\n\
         \x20   ]\n\
         \n\
         sumRuns rs = List.foldl (\\r acc -> acc + r.run ()) 0 rs\n\
         \n\
         main : String\n\
         main = String.fromInt (sumRuns runners)\n",
    );
}

#[test]
fn point_free_composition_partial_application() {
    // `(\\f (a,b) -> f a b) >> g` composed and applied — the left operand is a
    // multi-parameter lambda applied to one argument (partial), and `g` is a
    // point-free / partially-applied function. Mirrors elm-test's `fuzz2`.
    assert_same(
        "point_free_compose",
        "module Test exposing (..)\n\
         \n\
         mk : Int -> (( Int, Int ) -> Int) -> Int\n\
         mk k g = k + g ( 1, 2 )\n\
         \n\
         combine : Int -> (Int -> Int -> Int) -> Int\n\
         combine k = (\\f ( a, b ) -> f a b) >> mk k\n\
         \n\
         main : String\n\
         main = String.fromInt (combine 100 (\\a b -> a + b))\n",
    );
}

#[test]
fn deep_tail_recursion_with_allocation() {
    // A self-tail-recursive function must run in constant stack (the backend
    // compiles the tail self-call to a loop), even when its body allocates —
    // the heap allocation blocks LLVM's own tail-call elimination, so the
    // explicit tail-loop is required.
    assert_same(
        "deep_tail_rec_alloc",
        "module Test exposing (..)\n\
         \n\
         countDown : Int -> List Int -> Int\n\
         countDown n acc =\n\
         \x20   if n == 0 then List.length acc\n\
         \x20   else countDown (n - 1) (n :: acc)\n\
         \n\
         main : String\n\
         main = String.fromInt (countDown 200000 [])\n",
    );
}

#[test]
fn polymorphic_local_manipulates_payload() {
    // A polymorphic local `let` function whose payload flows unboxed through a
    // kernel (concatMap) must be specialized per use-site so its parameters get
    // the concrete unboxed layout, not a boxed Ref. permutations of [1,2,3].
    assert_same(
        "poly_local_payload",
        "module Test exposing (..)\n\
         \n\
         select : List a -> List ( a, List a )\n\
         select xs =\n\
         \x20   case xs of\n\
         \x20       [] -> []\n\
         \x20       x :: rest -> ( x, rest ) :: List.map (\\( y, ys ) -> ( y, x :: ys )) (select rest)\n\
         \n\
         perm : List a -> List (List a)\n\
         perm xs_ =\n\
         \x20   case xs_ of\n\
         \x20       [] -> [ [] ]\n\
         \x20       xs -> let f ( y, ys ) = List.map ((::) y) (perm ys) in List.concatMap f (select xs)\n\
         \n\
         main : String\n\
         main = Debug.toString (perm [ 1, 2, 3 ])\n",
    );
}

#[test]
fn polymorphic_local_used_at_two_types() {
    // A polymorphic local used at two distinct concrete types must be
    // specialized once per type (isJust-style).
    assert_same(
        "poly_local_two_types",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       isJust m = case m of\n\
         \x20           Just _ -> True\n\
         \x20           Nothing -> False\n\
         \x20   in\n\
         \x20   Debug.toString ( isJust (Just 3), isJust (Just 1.5), isJust Nothing )\n",
    );
}

#[test]
fn record_alias_constructors() {
    // A record type-alias used as a constructor (`User 10`) desugars to a
    // synthesized lambda/record/field-var tree. Those nodes must each get a
    // distinct region so the type checker's region-keyed `node_types` does not
    // clobber the record's type with a field's (e.g. an unresolved `number`),
    // which previously corrupted the typed backend's record layout. Exercises a
    // single-field ctor, a multi-field ctor with a nested record-ctor argument,
    // and a ctor used as a first-class function.
    assert_same(
        "record_alias_constructors",
        "module Test exposing (..)\n\
         \n\
         type alias User =\n\
         \x20   { age : Int }\n\
         \n\
         type alias UserWithCity =\n\
         \x20   { user : User, city : String }\n\
         \n\
         main : String\n\
         main =\n\
         \x20   Debug.toString\n\
         \x20       ( User 10\n\
         \x20       , UserWithCity (User 42) \"paris\"\n\
         \x20       , List.map (\\n -> User n) [ 1, 2, 3 ]\n\
         \x20       )\n",
    );
}

#[test]
fn nested_and_point_free_lambdas_through_hof() {
    // A closure's compiled arity must equal its type's total arrow count so a
    // higher-order caller that passes one argument per arrow does not read past
    // the closure's parameters. A directly-nested lambda (`\_ -> \b -> b + 1`)
    // must flatten to two parameters, and a point-free tail (`\_ -> identity`)
    // must eta-expand -- both previously returned garbage when applied with two
    // arguments at once.
    assert_same(
        "nested_point_free_lambdas",
        "module Test exposing (..)\n\
         \n\
         apply2 : (a -> b -> c) -> a -> b -> c\n\
         apply2 f x y =\n\
         \x20   f x y\n\
         \n\
         main : String\n\
         main =\n\
         \x20   Debug.toString\n\
         \x20       ( apply2 (\\_ b -> b + 1) 5 10\n\
         \x20       , apply2 (\\_ -> \\b -> b + 1) 5 10\n\
         \x20       , apply2 (\\_ -> identity) 5 99\n\
         \x20       )\n",
    );
}

#[test]
fn composition_and_partial_arity_normalization() {
    // Two more ways a closure's compiled arity could fall short of its type's
    // arrow count. `add << negate` composes into a two-argument function but the
    // composition closure is intrinsically arity-1, so it must eta-expand. And a
    // point-free partial `mkAdder 1` of a function whose own body returns a
    // function (`mkAdder x y = (+) (x + y)`) must remember it still needs two
    // more arguments, which requires the definition itself to be eta-normalized.
    assert_same(
        "composition_partial_arity",
        "module Test exposing (..)\n\
         \n\
         add : Int -> Int -> Int\n\
         add a b =\n\
         \x20   a + b\n\
         \n\
         composed : Int -> Int -> Int\n\
         composed =\n\
         \x20   add << negate\n\
         \n\
         mkAdder : Int -> Int -> (Int -> Int)\n\
         mkAdder x y =\n\
         \x20   (+) (x + y)\n\
         \n\
         partial : Int -> (Int -> Int)\n\
         partial =\n\
         \x20   mkAdder 1\n\
         \n\
         main : String\n\
         main =\n\
         \x20   Debug.toString ( composed 3 10, partial 10 100 )\n",
    );
}

#[test]
fn json_decode_and_encode() {
    // The native runtime implements Json.Decode/Json.Encode (reified decoders +
    // a JSON parser/serializer). Exercise decodeString through a map3 record
    // decoder, round-trip encode (compact + errorToString path), and a decode
    // failure — all must match the JS backend byte for byte.
    assert_same(
        "json_decode_and_encode",
        "module Test exposing (..)\n\
         \n\
         import Json.Decode as D\n\
         import Json.Encode as E\n\
         \n\
         type alias Rec =\n\
         \x20   { name : String, age : Int, tags : List String }\n\
         \n\
         decoder : D.Decoder Rec\n\
         decoder =\n\
         \x20   D.map3 Rec\n\
         \x20       (D.field \"name\" D.string)\n\
         \x20       (D.field \"age\" D.int)\n\
         \x20       (D.field \"tags\" (D.list D.string))\n\
         \n\
         enc : Rec -> E.Value\n\
         enc r =\n\
         \x20   E.object [ ( \"name\", E.string r.name ), ( \"age\", E.int r.age ), ( \"tags\", E.list E.string r.tags ) ]\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       decoded = D.decodeString decoder \"{\\\"name\\\":\\\"Ann\\\",\\\"age\\\":30,\\\"tags\\\":[\\\"a\\\",\\\"b\\\"]}\"\n\
         \x20       reEncoded = case decoded of\n\
         \x20           Ok r -> E.encode 0 (enc r)\n\
         \x20           Err e -> D.errorToString e\n\
         \x20       failMsg = case D.decodeString (D.field \"age\" D.string) \"{\\\"age\\\":1}\" of\n\
         \x20           Ok _ -> \"?\"\n\
         \x20           Err e -> D.errorToString e\n\
         \x20   in\n\
         \x20   String.join \"|\" [ Debug.toString decoded, reEncoded, failMsg, Debug.toString (D.decodeString (D.list D.int) \"[1,2,3]\") ]\n",
    );
}

#[test]
fn time_civil_date_math() {
    // The native runtime implements elm/time's civil-date math (toYear/toMonth/
    // toDay/toWeekday/toHour/toMinute/toSecond/toMillis + customZone offsets).
    // Exercise a UTC instant, an offset zone, and a pre-epoch (negative ms)
    // instant; all must match the JS backend.
    assert_same(
        "time_civil_date_math",
        "module Test exposing (..)\n\
         \n\
         import Time\n\
         \n\
         d : a -> String\n\
         d =\n\
         \x20   Debug.toString\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       t = Time.millisToPosix 1719792645123\n\
         \x20       z = Time.utc\n\
         \x20       east = Time.customZone 120 []\n\
         \x20   in\n\
         \x20   String.join \"|\"\n\
         \x20       [ d (Time.toYear z t), d (Time.toMonth z t), d (Time.toDay z t)\n\
         \x20       , d (Time.toWeekday z t), d (Time.toHour z t), d (Time.toMinute z t)\n\
         \x20       , d (Time.toSecond z t), d (Time.toMillis z t), d (Time.toHour east t)\n\
         \x20       , d (Time.toYear z (Time.millisToPosix -1)), d (Time.toMonth z (Time.millisToPosix -1))\n\
         \x20       ]\n",
    );
}

#[test]
fn unit_equality() {
    // Unit has one inhabitant, so `() == ()` is always true; equality on Unit
    // (directly, nested in a tuple, and in a list) must match the JS backend.
    assert_same(
        "unit_equality",
        "module Test exposing (..)\n\
         \n\
         d : a -> String\n\
         d =\n\
         \x20   Debug.toString\n\
         \n\
         main : String\n\
         main =\n\
         \x20   String.join \"|\"\n\
         \x20       [ d (() == ()), d (() /= ()), d (( (), 1 ) == ( (), 1 )), d ([ () ] == [ () ]) ]\n",
    );
}

#[test]
fn custom_infix_operator() {
    // A library-defined operator (like elm/parser's `|=`/`|.`) is not one of
    // the typed backend's built-in binops; it must lower to a plain call of its
    // resolving function. Previously such an operator reached gen_binop and
    // crashed trying to read a non-numeric operand as an integer.
    assert_same(
        "custom_infix_operator",
        "module Test exposing (..)\n\
         \n\
         infix left 5 (|+|) = combine\n\
         \n\
         combine : Int -> Int -> Int\n\
         combine a b =\n\
         \x20   a * 10 + b\n\
         \n\
         main : String\n\
         main =\n\
         \x20   Debug.toString ( 1 |+| 2 |+| 3, 5 |+| 0 )\n",
    );
}

#[test]
fn point_free_polymorphic_let_binding() {
    // A point-free (zero-argument) local binding whose type is polymorphic in
    // context -- `f = List.sort << dedup` -- must be specialized per use-site
    // like any polymorphic local function. Compiling it once left its type
    // variable free, so the functions it composed got boxed (Ref) instead of
    // unboxed Int specializations, and the concrete Int call site then read the
    // list elements at the wrong layout (they came out as Unit). Exercises the
    // binding used at a concrete Int type.
    assert_same(
        "point_free_poly_let",
        "module Test exposing (..)\n\
         \n\
         dedup : List a -> List a\n\
         dedup xs =\n\
         \x20   List.foldr (\\x acc -> if List.member x acc then acc else x :: acc) [] xs\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       sorted =\n\
         \x20           List.sort << dedup\n\
         \x20   in\n\
         \x20   Debug.toString ( sorted [ 3, 1, 2, 1, 3 ], sorted [ 5, 2, 5 ] )\n",
    );
}

#[test]
fn nested_pattern_match_on_recursive_field() {
    // Pattern-matching a constructor whose field is the recursive type itself
    // (e.g. `Node (Node _ _) _`) reaches that field at `Ref` layout (recursion
    // is broken with a pointer). match_pattern was layout-directed and bailed
    // with \"case on layout Ref is not supported\"; it is now type-directed and
    // recovers the constructors' field types from the concrete union, so a
    // nested pattern on a recursive field matches. Exercises a deep nested
    // match plus a `::` on a recursive-typed list element.
    assert_same(
        "recursive_case",
        "module Test exposing (..)\n\
         \n\
         type Tree = Leaf Int | Node Tree Tree\n\
         \n\
         build : Int -> Tree\n\
         build n =\n\
         \x20   if n <= 0 then Leaf n else Node (build (n - 1)) (build (n - 1))\n\
         \n\
         describe : Tree -> String\n\
         describe t =\n\
         \x20   case t of\n\
         \x20       Node (Node _ _) (Leaf x) ->\n\
         \x20           \"nn-leaf \" ++ String.fromInt x\n\
         \n\
         \x20       Node (Leaf a) _ ->\n\
         \x20           \"nleaf \" ++ String.fromInt a\n\
         \n\
         \x20       Node _ _ ->\n\
         \x20           \"node\"\n\
         \n\
         \x20       Leaf n ->\n\
         \x20           \"leaf \" ++ String.fromInt n\n\
         \n\
         main : String\n\
         main =\n\
         \x20   [ describe (build 0), describe (build 1), describe (build 2), describe (build 3) ]\n\
         \x20       |> String.join \"|\"\n",
    );
}

#[test]
fn structural_equality_of_recursive_values() {
    // A recursive type's layout uses `Ref` to break self-reference, and the
    // layout-directed comparison bottomed out at `Ref` as pointer identity — so
    // two structurally-equal but distinct recursive values (trees, linked
    // lists, JSON:API resources) compared unequal. Equality is now type-directed
    // with a memoized per-type helper: it compares in place (no allocation) and
    // breaks recursion at the function level. Checks equal trees compare equal,
    // unequal trees unequal, and a recursive value nested in a record.
    assert_same(
        "recursive_eq",
        "module Test exposing (..)\n\
         \n\
         type Tree = Leaf Int | Node Tree Tree\n\
         \n\
         build : Int -> Tree\n\
         build n =\n\
         \x20   if n <= 0 then Leaf n else Node (build (n - 1)) (build (n - 1))\n\
         \n\
         main : String\n\
         main =\n\
         \x20   [ Debug.toString (build 4 == build 4)\n\
         \x20   , Debug.toString (build 4 == Node (build 3) (Leaf 9))\n\
         \x20   , Debug.toString ({ t = build 3, n = 1 } == { t = build 3, n = 1 })\n\
         \x20   , Debug.toString (Just (build 3) == Just (build 3))\n\
         \x20   ]\n\
         \x20       |> String.join \"|\"\n",
    );
}

#[test]
fn string_trim_unicode_whitespace() {
    // Elm's `String.trim` delegates to JS `String.prototype.trim`, whose
    // whitespace set is wider than ASCII — it includes the non-breaking space
    // (U+00A0), the Unicode `Zs` category, line/paragraph separators, and the
    // BOM. Native trimmed only ASCII whitespace, so `String.trim "\u{00A0}"`
    // was non-empty (elm-csv's `blank` decoder then failed to treat an
    // all-whitespace field as blank).
    assert_same(
        "trim_unicode_ws",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   [ Debug.toString (String.trim \"\\u{00A0}\\t\\n\" == \"\")\n\
         \x20   , String.trim \"\\u{00A0}\\u{2003}x y\\u{3000}\"\n\
         \x20   , String.fromInt (String.length (String.trim \"\\u{FEFF}\\u{00A0}\"))\n\
         \x20   ]\n\
         \x20       |> String.join \"|\"\n",
    );
}

#[test]
fn local_row_polymorphic_record_access() {
    // A record's fields are laid out sorted by name, so the offset of a field
    // depends on the *full* field set. A row-polymorphic function
    // (`{ r | ... } -> ...`) must therefore be specialized to the concrete
    // record at each use site, or its field accesses read the offsets of the
    // partial (open) field set. Top-level functions were specialized, but a
    // *local* `let`-bound row-polymorphic function was not — its open-record
    // parameter was treated as monomorphic, so `loc.startRow` read a
    // neighbouring field (a pointer) as an Int (elm-csv's `errorToString`).
    // Here `mid` sorts between `lo` and `zz`, so the extra fields shift its
    // offset; the local `get` must still read `mid`, not its neighbour.
    assert_same(
        "local_row_poly_access",
        "module Test exposing (..)\n\
         \n\
         type Tag = Tag String\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       get loc =\n\
         \x20           String.fromInt loc.mid ++ \"/\" ++ String.fromInt loc.lo\n\
         \x20   in\n\
         \x20   get { lo = 3, mid = 7, zz = 99, tag = Tag \"x\" }\n",
    );
}

#[test]
fn function_identity_survives_uniform_roundtrip() {
    // A function stored in a uniform container (Dict) is boxed; reading it back
    // unboxes it. box∘unbox must preserve identity so the round-tripped
    // function still compares equal to the original (Elm's `==` on functions is
    // reference equality) — the case that kept Evelios/elm-markov's encode/decode
    // roundtrip failing (its model embeds a comparator function). Compared
    // through a Dict so equality goes via the runtime's value_eq.
    assert_same(
        "fn_roundtrip_identity",
        "module Test exposing (..)\n\
         \n\
         import Dict\n\
         \n\
         bump : Int -> Int\n\
         bump x =\n\
         \x20   x + 1\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       got =\n\
         \x20           Dict.get 0 (Dict.fromList [ ( 0, bump ) ])\n\
         \x20   in\n\
         \x20   Debug.toString (Dict.singleton 1 got == Dict.singleton 1 (Just bump))\n",
    );
}

#[test]
fn equality_of_values_containing_a_shared_function() {
    // Elm's `==` short-circuits to `true` for the same function reference, so a
    // data structure that embeds a function (here a `Dict` value) compares equal
    // when it holds the same top-level function. Native builds a fresh closure
    // per reference, so this only works if references to a top-level function
    // are canonical (a shared global closure) and the runtime's `value_eq`
    // compares closures by function pointer + captures rather than falling
    // through to `false`. Exercises both fixes.
    assert_same(
        "eq_shared_function",
        "module Test exposing (..)\n\
         \n\
         import Dict\n\
         \n\
         bump : Int -> Int\n\
         bump x =\n\
         \x20   x + 1\n\
         \n\
         main : String\n\
         main =\n\
         \x20   Debug.toString\n\
         \x20       ( Dict.fromList [ ( 0, bump ) ] == Dict.fromList [ ( 0, bump ) ]\n\
         \x20       , Dict.fromList [ ( 0, bump ) ] == Dict.fromList [ ( 1, bump ) ]\n\
         \x20       )\n",
    );
}

#[test]
fn hoisted_closed_local_constant() {
    // A `let` value inside a function whose right-hand side depends on none of
    // the function's arguments is a constant relative to every call. The typed
    // backend hoists such a closed, allocating binding to a memoized global so
    // it is built once, not per call (or per loop iteration) — the same
    // generalization as a top-level CAF, reached inside a function. Here the
    // lookup `table` is closed inside `lookup`, which is applied across a list;
    // this checks the hoisted value stays correct. (The performance/memory win
    // — rebuilding the table every call vs once — is the point, but only the
    // result is observable in a differential test.)
    assert_same(
        "hoisted_closed_local",
        "module Test exposing (..)\n\
         \n\
         import Array exposing (Array)\n\
         \n\
         lookup : Int -> Int\n\
         lookup i =\n\
         \x20   let\n\
         \x20       table : Array Int\n\
         \x20       table =\n\
         \x20           List.range 0 20 |> List.map (\\x -> x * x) |> Array.fromList\n\
         \x20   in\n\
         \x20   Array.get (modBy 21 i) table |> Maybe.withDefault 0\n\
         \n\
         main : String\n\
         main =\n\
         \x20   List.range 0 50 |> List.map lookup |> List.sum |> String.fromInt\n",
    );
}

#[test]
fn memoized_top_level_constant() {
    // A top-level nullary value (a CAF) is an Elm constant, evaluated once.
    // The typed backend memoizes it behind a module global so referencing it
    // many times does not recompute it (which for a heap value like this
    // lookup array would rebuild the whole structure per access and, under the
    // non-freeing bump allocator, grow without bound — the elm-secret-sharing
    // GF256-tables OOM). This checks the memoized value stays correct across
    // repeated references in a fold.
    assert_same(
        "memoized_caf",
        "module Test exposing (..)\n\
         \n\
         import Array exposing (Array)\n\
         \n\
         table : Array Int\n\
         table =\n\
         \x20   List.range 0 20 |> List.map (\\x -> x * x) |> Array.fromList\n\
         \n\
         main : String\n\
         main =\n\
         \x20   List.range 0 100\n\
         \x20       |> List.foldl (\\i acc -> acc + (Array.get (modBy 21 i) table |> Maybe.withDefault 0)) 0\n\
         \x20       |> String.fromInt\n",
    );
}

#[test]
fn boxed_closure_arity_above_twelve() {
    // A function with many parameters, boxed as a uniform closure and applied
    // through the uniform path (here via a `Dict` value slot), calls the
    // runtime `call_fn` with arity = params + 1 (the captured-closure word that
    // `box_closure`'s trampoline prepends). A 12-parameter function therefore
    // needs `call_fn(13)`; the runtime originally capped at 12 and crashed with
    // "function arity too large" (Holmusk/swagger-decoder's 12-field record
    // decoders). `call_fn` now handles up to 32 and the stack argument buffers
    // are sized to match.
    assert_same(
        "boxed_closure_arity_13",
        "module Test exposing (..)\n\
         \n\
         import Dict\n\
         \n\
         add12 : Int -> Int -> Int -> Int -> Int -> Int -> Int -> Int -> Int -> Int -> Int -> Int -> Int\n\
         add12 a b c d e f g h i j k l =\n\
         \x20   a + b + c + d + e + f + g + h + i + j + k + l\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       d =\n\
         \x20           Dict.fromList [ ( \"f\", add12 ) ]\n\
         \x20   in\n\
         \x20   case Dict.get \"f\" d of\n\
         \x20       Just fn ->\n\
         \x20           String.fromInt (fn 1 2 3 4 5 6 7 8 9 10 11 12)\n\
         \n\
         \x20       Nothing ->\n\
         \x20           \"no\"\n",
    );
}

#[test]
fn point_free_local_function_in_higher_order_kernel() {
    // A local `let` function that is point-free in its last argument --
    // `prepend x = List.append [x]`, whose type is `Int -> List Int -> List Int`
    // but which declares only one parameter -- must be eta-normalized to its
    // full type arity, exactly as top-level functions are. `List.foldl` calls
    // its function with one argument per arrow (two here); a compiled arity of
    // one made the fold apply `prepend` with more arguments than it had
    // parameters, reading past the closure. This is the arity mismatch that
    // segfaulted elm-explorations/test's `Test.Internal.duplicatedName`
    // (`insertOrFail newName = Result.andThen (...)`, folded over test labels),
    // crashing every native-typed run of a package with a `describe` block.
    assert_same(
        "point_free_local_in_hof",
        "module Test exposing (..)\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       prepend : Int -> List Int -> List Int\n\
         \x20       prepend x =\n\
         \x20           List.append [ x ]\n\
         \x20   in\n\
         \x20   Debug.toString (List.foldl prepend [] [ 1, 2, 3 ])\n",
    );
}

#[test]
fn partial_record_alias_constructor() {
    // A record type-alias used as a constructor and *partially* applied
    // (`succeed (T inc) |> andMap ...`, the elm/json-extra andMap idiom) must
    // be typed as a function, not the full record. The synthesized desugaring
    // nodes' regions previously collided with the enclosing application node's
    // region in node_types, so the partial application read as the full record
    // -- a closure then boxed as a record (compile panic) or applied as a
    // non-function (runtime crash). Exercises a 3-field record built field by
    // field through a partial constructor.
    assert_same(
        "partial_record_alias_ctor",
        "module Test exposing (..)\n\
         \n\
         andMap : Maybe a -> Maybe (a -> b) -> Maybe b\n\
         andMap =\n\
         \x20   Maybe.map2 (|>)\n\
         \n\
         type alias T =\n\
         \x20   { inc : List Int, x : String, y : String }\n\
         \n\
         build : List Int -> Maybe T\n\
         build inc =\n\
         \x20   Just (T inc)\n\
         \x20       |> andMap (Just \"a\")\n\
         \x20       |> andMap (Just \"b\")\n\
         \n\
         main : String\n\
         main =\n\
         \x20   Debug.toString (build [ 1, 2 ])\n",
    );
}

#[test]
fn native_html_structural_equality() {
    // elm/html values are ordinary comparable data in the native backend
    // (constructors, no event handlers), matching how elm-explorations/test
    // packages like ChristophP/elm-mark and FordLabs/elm-star-rating compare
    // built `Html`/`Attribute` values with `Expect.equal`. Exercises text/node
    // equality, child inequality, the className merge (`_VDom_organize`), and
    // attribute equality — all against the JS backend.
    assert_same(
        "native_html_eq",
        "module Test exposing (..)\n\
         \n\
         import Html exposing (div, text, mark)\n\
         import Html.Attributes exposing (class, style)\n\
         \n\
         b : Bool -> String\n\
         b x = if x then \"T\" else \"F\"\n\
         \n\
         main : String\n\
         main =\n\
         \x20   String.concat\n\
         \x20       [ b (text \"a\" == text \"a\")\n\
         \x20       , b (text \"a\" == text \"b\")\n\
         \x20       , b (mark [] [ text \"x\" ] == mark [] [ text \"x\" ])\n\
         \x20       , b (div [ class \"a\", class \"b\" ] [] == div [ class \"a b\" ] [])\n\
         \x20       , b ([ style \"o\" \"1\" ] == [ style \"o\" \"1\" ])\n\
         \x20       , b (class \"a\" == class \"a\")\n\
         \x20       , b (div [] [ text \"a\" ] == div [] [ text \"b\" ])\n\
         \x20       ]\n",
    );
}

#[test]
fn multi_arg_function_mapped_point_free() {
    // `List.map` over a 2-argument function used point-free yields a list of
    // partial closures (`List (Int -> Int)`), which are then each applied. The
    // typed backend previously called the 2-ary function with a single argument
    // (invalid IR) instead of building a partial closure. This is the bug that
    // broke elm-explorations/test's `List.map predicateFromSelector selectors`.
    assert_same(
        "map_point_free_binary",
        "module Test exposing (..)\n\
         \n\
         add : Int -> Int -> Int\n\
         add a b = a + b\n\
         \n\
         main : String\n\
         main =\n\
         \x20   let\n\
         \x20       fns = List.map add [ 1, 2, 3 ]\n\
         \x20   in\n\
         \x20   Debug.toString (List.map (\\f -> f 10) fns)\n",
    );
}
