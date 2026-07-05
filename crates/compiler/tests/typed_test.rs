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
