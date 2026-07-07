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
