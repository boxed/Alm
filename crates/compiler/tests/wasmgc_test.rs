//! WasmGC backend (experimental). Compiles `Test.main` with the WasmGC code
//! generator, runs the module under Node, and checks the result matches the JS
//! backend. Phase W1 covers integer programs only.

mod common;

use std::process::Command;

use alm_compiler::{generate, project};

const RUNNER: &str = r#"
const fs = require('fs');
const bytes = fs.readFileSync(process.argv[2]);
const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {});
console.log(instance.exports.main_int().toString());
"#;

// Runs `render()` (for `main : String`), reading the UTF-8 bytes back out of
// the module's linear memory.
const STR_RUNNER: &str = r#"
const fs = require('fs');
const bytes = fs.readFileSync(process.argv[2]);
const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {});
const len = instance.exports.render();
const mem = new Uint8Array(instance.exports.memory.buffer, 0, len);
process.stdout.write(Buffer.from(mem).toString('utf8'));
"#;

/// Compile a whole module whose `main : String` and assert both backends agree.
fn assert_str_prog(test_name: &str, source: &str) {
    let dir = common::test_dir("alm-wasmgc", test_name);
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");

    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });

    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("write bundle");
    let js = run(Command::new("node").arg("-e").arg(format!(
        "process.stdout.write(require({:?}).Test.main)",
        bundle.display()
    )));

    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let runner = dir.join("run_str.cjs");
    std::fs::write(&runner, STR_RUNNER).expect("write runner");
    let wasm_out = run(Command::new("node").arg(&runner).arg(&wasm));

    assert_eq!(js, wasm_out, "JS and WasmGC backends disagree");
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("spawn node");
    assert!(
        out.status.success(),
        "node failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

/// Compile `main = <int expr>` with both backends and assert they agree.
fn assert_int(test_name: &str, expr: &str) {
    let source = format!("module Test exposing (main)\n\nmain : Int\nmain =\n    {expr}\n");
    assert_int_prog(test_name, &source);
}

/// Compile a whole module whose `main : Int` and assert both backends agree.
fn assert_int_prog(test_name: &str, source: &str) {
    let dir = common::test_dir("alm-wasmgc", test_name);
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, &source).expect("write fixture");

    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!(
            "check failed:\n{}",
            errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
        )
    });

    // JS backend.
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("write bundle");
    let js = run(Command::new("node").arg("-e").arg(format!(
        "console.log(require({:?}).Test.main)",
        bundle.display()
    )));

    // WasmGC backend.
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!(
            "wasmgc build failed:\n{}",
            e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
        )
    });
    let runner = dir.join("run.cjs");
    std::fs::write(&runner, RUNNER).expect("write runner");
    let wasm_out = run(Command::new("node").arg(&runner).arg(&wasm));

    assert_eq!(js, wasm_out, "JS and WasmGC backends disagree");
}

#[test]
fn int_add() {
    assert_int("add", "40 + 2");
}

#[test]
fn int_arith() {
    assert_int("arith", "(10 - 3) * 6 // 2");
}

#[test]
fn int_negate() {
    assert_int("negate", "-5 + 10");
}

#[test]
fn int_if() {
    assert_int("if", "if 3 > 2 then 100 else 0");
}

#[test]
fn int_if_chain() {
    assert_int("if_chain", "if 1 > 2 then 10 else if 5 == 5 then 20 else 30");
}

#[test]
fn recursion_factorial() {
    assert_int_prog(
        "factorial",
        "module Test exposing (main)\n\n\
         fact : Int -> Int\n\
         fact n = if n <= 1 then 1 else n * fact (n - 1)\n\n\
         main : Int\n\
         main = fact 10\n",
    );
}

#[test]
fn recursion_fib() {
    assert_int_prog(
        "fib",
        "module Test exposing (main)\n\n\
         fib : Int -> Int\n\
         fib n = if n < 2 then n else fib (n - 1) + fib (n - 2)\n\n\
         main : Int\n\
         main = fib 20\n",
    );
}

#[test]
fn let_bindings() {
    assert_int_prog(
        "let",
        "module Test exposing (main)\n\n\
         main : Int\n\
         main =\n    let\n        x = 10\n        y = x * x\n    in\n    x + y\n",
    );
}

#[test]
fn multi_arg_and_helpers() {
    assert_int_prog(
        "multiarg",
        "module Test exposing (main)\n\n\
         addThree : Int -> Int -> Int -> Int\n\
         addThree a b c = a + b + c\n\n\
         main : Int\n\
         main = addThree 1 20 300\n",
    );
}

#[test]
fn string_literal() {
    assert_str_prog(
        "str_lit",
        "module Test exposing (main)\n\nmain : String\nmain = \"hello world\"\n",
    );
}

#[test]
fn string_append() {
    assert_str_prog(
        "str_append",
        "module Test exposing (main)\n\nmain : String\nmain = \"foo\" ++ \"-\" ++ \"bar\"\n",
    );
}

#[test]
fn string_from_int() {
    assert_str_prog(
        "str_from_int",
        "module Test exposing (main)\n\n\
         main : String\n\
         main = \"n=\" ++ String.fromInt (6 * 7)\n",
    );
}

#[test]
fn string_from_int_negative() {
    assert_str_prog(
        "str_from_int_neg",
        "module Test exposing (main)\n\n\
         main : String\n\
         main = String.fromInt (0 - 12345)\n",
    );
}

#[test]
fn string_from_int_zero() {
    assert_str_prog(
        "str_from_int_zero",
        "module Test exposing (main)\n\nmain : String\nmain = String.fromInt 0\n",
    );
}

#[test]
fn string_recursive_build() {
    assert_str_prog(
        "str_rec",
        "module Test exposing (main)\n\n\
         range : Int -> String\n\
         range n = if n <= 0 then \"\" else range (n - 1) ++ String.fromInt n\n\n\
         main : String\n\
         main = range 5\n",
    );
}

#[test]
fn custom_type_case() {
    assert_str_prog(
        "color",
        "module Test exposing (main)\n\n\
         type Color = Red | Green | Blue\n\n\
         name : Color -> String\n\
         name c = case c of\n            Red -> \"red\"\n            Green -> \"green\"\n            Blue -> \"blue\"\n\n\
         main : String\n\
         main = name Green ++ \"-\" ++ name Blue\n",
    );
}

#[test]
fn maybe_case() {
    assert_str_prog(
        "maybe",
        "module Test exposing (main)\n\n\
         describe : Maybe Int -> String\n\
         describe m = case m of\n            Just n -> \"just \" ++ String.fromInt n\n            Nothing -> \"nothing\"\n\n\
         main : String\n\
         main = describe (Just 42) ++ \"/\" ++ describe Nothing\n",
    );
}

#[test]
fn list_sum_recursive() {
    assert_str_prog(
        "list_sum",
        "module Test exposing (main)\n\n\
         sum : List Int -> Int\n\
         sum xs = case xs of\n            [] -> 0\n            x :: rest -> x + sum rest\n\n\
         main : String\n\
         main = String.fromInt (sum [1, 2, 3, 4, 5])\n",
    );
}

#[test]
fn tuple_case() {
    assert_str_prog(
        "tuple",
        "module Test exposing (main)\n\n\
         main : String\n\
         main = case ( 3, 4 ) of\n        ( a, b ) -> String.fromInt (a * b)\n",
    );
}

#[test]
fn record_access() {
    assert_str_prog(
        "record",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    let\n        p = { x = 7, y = 9 }\n    in\n    String.fromInt (p.x + p.y)\n",
    );
}

#[test]
fn nested_case_and_lists() {
    assert_str_prog(
        "nested",
        "module Test exposing (main)\n\n\
         firstTwo : List Int -> String\n\
         firstTwo xs = case xs of\n            a :: b :: _ -> String.fromInt a ++ \",\" ++ String.fromInt b\n            [ a ] -> String.fromInt a\n            [] -> \"empty\"\n\n\
         main : String\n\
         main = firstTwo [ 10, 20, 30 ] ++ \"|\" ++ firstTwo [ 99 ] ++ \"|\" ++ firstTwo []\n",
    );
}

#[test]
fn higher_order_apply() {
    assert_str_prog(
        "hof_apply",
        "module Test exposing (main)\n\n\
         apply : (Int -> Int) -> Int -> Int\n\
         apply f x = f x\n\n\
         inc : Int -> Int\n\
         inc n = n + 1\n\n\
         main : String\n\
         main = String.fromInt (apply inc 5)\n",
    );
}

#[test]
fn partial_application() {
    assert_str_prog(
        "partial",
        "module Test exposing (main)\n\n\
         add : Int -> Int -> Int\n\
         add a b = a + b\n\n\
         main : String\n\
         main =\n    let\n        add5 = add 5\n    in\n    String.fromInt (add5 10 + add5 100)\n",
    );
}

#[test]
fn pipeline() {
    assert_str_prog(
        "pipeline",
        "module Test exposing (main)\n\n\
         double : Int -> Int\n\
         double n = n * 2\n\n\
         main : String\n\
         main = String.fromInt (5 |> double |> double)\n",
    );
}

#[test]
fn list_map_lambda() {
    assert_str_prog(
        "list_map",
        "module Test exposing (main)\n\n\
         join : List Int -> String\n\
         join xs = List.foldl (\\n acc -> acc ++ String.fromInt n ++ \",\") \"\" xs\n\n\
         main : String\n\
         main = join (List.map (\\x -> x * x) [ 1, 2, 3, 4 ])\n",
    );
}

#[test]
fn list_length_and_fold() {
    assert_str_prog(
        "list_len",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    let\n        xs = [ 5, 10, 15 ]\n    in\n    String.fromInt (List.length xs) ++ \":\" ++ String.fromInt (List.foldl (+) 0 xs)\n",
    );
}

#[test]
fn floats_and_equality() {
    assert_str_prog(
        "floats",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    let\n        x = 3.0 * 2.5\n        r = Basics.round (x + 0.5)\n    in\n    String.fromInt r ++ \"/\" ++ String.fromInt (if x == 7.5 then 1 else 0)\n",
    );
}

#[test]
fn structural_equality() {
    assert_str_prog(
        "eq",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    let\n        a = [ 1, 2, 3 ] == [ 1, 2, 3 ]\n        b = ( \"x\", 5 ) == ( \"x\", 6 )\n        c = Just 3 == Just 3\n    in\n    String.fromInt (if a then 1 else 0) ++ String.fromInt (if b then 1 else 0) ++ String.fromInt (if c then 1 else 0)\n",
    );
}

#[test]
fn bool_ops_and_conversions() {
    assert_str_prog(
        "boolops",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    let\n        t = (1 < 2) && not (3 < 2)\n        n = round (toFloat 7 / 2.0)\n    in\n    String.fromInt (if t || False then n else 0)\n",
    );
}

#[test]
fn list_append_and_char() {
    assert_str_prog(
        "listapp",
        "module Test exposing (main)\n\n\
         showList : List Int -> String\n\
         showList xs = List.foldl (\\n acc -> acc ++ String.fromInt n) \"\" xs\n\n\
         main : String\n\
         main = showList ([ 1, 2 ] ++ [ 3, 4 ]) ++ String.fromInt (Char.toCode 'A')\n",
    );
}

#[test]
fn list_reverse_filter_foldr() {
    assert_str_prog(
        "list_rff",
        "module Test exposing (main)\n\n\
         show : List Int -> String\n\
         show xs = List.foldr (\\n acc -> String.fromInt n ++ acc) \"\" xs\n\n\
         main : String\n\
         main =\n    let\n        xs = [ 1, 2, 3, 4, 5, 6 ]\n        evens = List.filter (\\n -> modBy 2 n == 0) xs\n    in\n    show (List.reverse xs) ++ \"|\" ++ show evens\n",
    );
}

#[test]
fn list_range_member_concat() {
    assert_str_prog(
        "list_rmc",
        "module Test exposing (main)\n\n\
         sumStr : List Int -> String\n\
         sumStr xs = List.foldl (\\n acc -> acc ++ String.fromInt n ++ \",\") \"\" xs\n\n\
         main : String\n\
         main =\n    let\n        r = List.range 1 5\n        c = List.concat [ [ 1, 2 ], [ 3 ], [ 4, 5 ] ]\n    in\n    sumStr r ++ \"|\" ++ sumStr c ++ \"|\" ++ (if List.member 3 r then \"yes\" else \"no\")\n",
    );
}

#[test]
fn list_take_drop() {
    assert_str_prog(
        "list_td",
        "module Test exposing (main)\n\n\
         show : List Int -> String\n\
         show xs = List.foldl (\\n acc -> acc ++ String.fromInt n) \"\" xs\n\n\
         main : String\n\
         main =\n    let\n        xs = List.range 1 9\n    in\n    show (List.take 3 xs) ++ \"|\" ++ show (List.drop 6 xs)\n",
    );
}

#[test]
fn basics_abs_min_max_negate() {
    assert_str_prog(
        "abs_minmax",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    String.fromInt (abs (0 - 7))\n        ++ \",\" ++ String.fromInt (min 3 8)\n        ++ \",\" ++ String.fromInt (max 3 8)\n        ++ \",\" ++ String.fromInt (negate 4)\n",
    );
}

#[test]
fn maybe_tuple_head() {
    assert_str_prog(
        "maybe_tuple",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    let\n        xs = [ 10, 20, 30 ]\n        h = Maybe.withDefault 0 (List.head xs)\n        doubled = Maybe.map (\\n -> n * 2) (List.head xs)\n        p = ( 7, 9 )\n    in\n    String.fromInt h\n        ++ \",\" ++ String.fromInt (Maybe.withDefault 0 doubled)\n        ++ \",\" ++ String.fromInt (Tuple.first p + Tuple.second p)\n",
    );
}

#[test]
fn char_classifiers() {
    assert_str_prog(
        "char_class",
        "module Test exposing (main)\n\n\
         classify : Char -> String\n\
         classify c =\n    if Char.isDigit c then \"d\" else if Char.isLower c then \"l\" else if Char.isUpper c then \"u\" else \"?\"\n\n\
         main : String\n\
         main = classify '5' ++ classify 'a' ++ classify 'Z' ++ classify '!'\n",
    );
}

#[test]
fn maybe_nothing_paths() {
    assert_str_prog(
        "maybe_nothing",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    let\n        empty = List.drop 5 [ 1, 2, 3 ]\n    in\n    String.fromInt (Maybe.withDefault -1 (List.head empty))\n        ++ \",\" ++ String.fromInt (Maybe.withDefault -1 (Maybe.map (\\n -> n + 1) (List.head empty)))\n",
    );
}

#[test]
fn string_join_repeat_affix() {
    assert_str_prog(
        "str_jra",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    String.join \", \" [ \"a\", \"b\", \"c\" ]\n        ++ \"|\" ++ String.repeat 3 \"ab\"\n        ++ \"|\" ++ (if String.startsWith \"foo\" \"foobar\" then \"y\" else \"n\")\n        ++ (if String.endsWith \"bar\" \"foobar\" then \"y\" else \"n\")\n        ++ (if String.startsWith \"xyz\" \"foobar\" then \"y\" else \"n\")\n",
    );
}

#[test]
fn string_join_from_ints() {
    assert_str_prog(
        "str_join_ints",
        "module Test exposing (main)\n\n\
         main : String\n\
         main = String.join \"-\" (List.map String.fromInt (List.range 1 5))\n",
    );
}

#[test]
fn sort_and_compare() {
    assert_str_prog(
        "sort",
        "module Test exposing (main)\n\n\
         show : List Int -> String\n\
         show xs = String.join \",\" (List.map String.fromInt xs)\n\n\
         main : String\n\
         main =\n    show (List.sort [ 5, 2, 8, 1, 9, 3 ])\n        ++ \"|\" ++ String.fromInt (min 7 3)\n        ++ \",\" ++ String.fromInt (max 7 3)\n",
    );
}

#[test]
fn sort_strings() {
    assert_str_prog(
        "sort_str",
        "module Test exposing (main)\n\n\
         main : String\n\
         main = String.join \" \" (List.sort [ \"banana\", \"apple\", \"cherry\" ])\n",
    );
}

#[test]
fn compare_min_max_float_string() {
    assert_str_prog(
        "cmp_misc",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    (if min \"abc\" \"abd\" == \"abc\" then \"y\" else \"n\")\n        ++ (if max 2.5 1.5 == 2.5 then \"y\" else \"n\")\n",
    );
}

#[test]
fn list_all_any() {
    assert_str_prog(
        "all_any",
        "module Test exposing (main)\n\n\
         yn : Bool -> String\n\
         yn b = if b then \"y\" else \"n\"\n\n\
         main : String\n\
         main =\n    \
            yn (List.all (\\n -> n > 0) [ 1, 2, 3 ])\n        ++ yn (List.all (\\n -> n > 1) [ 1, 2, 3 ])\n        ++ yn (List.any (\\n -> n > 2) [ 1, 2, 3 ])\n        ++ yn (List.any (\\n -> n > 9) [ 1, 2, 3 ])\n        ++ yn (List.all (\\n -> n > 0) [])\n        ++ yn (List.any (\\n -> n > 0) [])\n",
    );
}

#[test]
fn list_min_max() {
    assert_str_prog(
        "minmax",
        "module Test exposing (main)\n\n\
         show : Maybe Int -> String\n\
         show m =\n    case m of\n        Just n ->\n            String.fromInt n\n\n        Nothing ->\n            \"-\"\n\n\
         main : String\n\
         main =\n    \
            show (List.minimum [ 5, 2, 8, 1, 9 ])\n        ++ \",\" ++ show (List.maximum [ 5, 2, 8, 1, 9 ])\n        ++ \",\" ++ show (List.minimum [])\n",
    );
}

#[test]
fn list_indexed_map() {
    assert_str_prog(
        "imap",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            [ \"a\", \"b\", \"c\" ]\n        |> List.indexedMap (\\i s -> String.fromInt i ++ s)\n        |> String.join \",\"\n",
    );
}

#[test]
fn list_sum_product() {
    assert_int_prog(
        "sumprod",
        "module Test exposing (main)\n\n\
         main : Int\n\
         main = List.sum [ 1, 2, 3, 4 ] + List.product [ 1, 2, 3, 4 ] + List.sum []\n",
    );
}

#[test]
fn list_sum_float() {
    assert_str_prog(
        "sumf",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            if List.sum [ 1.5, 2.5, 4.0 ] == 8.0 then\n        \"y\"\n\n    else\n        \"n\"\n",
    );
}

#[test]
fn list_partition_unzip() {
    assert_str_prog(
        "partition_unzip",
        "module Test exposing (main)\n\n\
         ints : List Int -> String\n\
         ints xs = String.join \",\" (List.map String.fromInt xs)\n\n\
         main : String\n\
         main =\n    \
            let\n        ( evens, odds ) =\n            List.partition (\\n -> modBy 2 n == 0) [ 1, 2, 3, 4, 5 ]\n\n        ( nums, strs ) =\n            List.unzip [ ( 1, \"a\" ), ( 2, \"b\" ), ( 3, \"c\" ) ]\n    in\n    ints evens ++ \"|\" ++ ints odds ++ \"|\" ++ ints nums ++ \"|\" ++ String.join \",\" strs\n",
    );
}

#[test]
fn bitwise_and_float_predicates() {
    assert_str_prog(
        "bitwise",
        "module Test exposing (main)\n\n\
         yn : Bool -> String\n\
         yn b = if b then \"y\" else \"n\"\n\n\
         main : String\n\
         main =\n    \
            String.join \",\"\n        (List.map String.fromInt\n            [ Bitwise.and 12 10\n            , Bitwise.or 12 10\n            , Bitwise.xor 12 10\n            , Bitwise.complement 0\n            , Bitwise.shiftLeftBy 2 1\n            , Bitwise.shiftRightBy 1 8\n            , Bitwise.shiftRightZfBy 1 -1\n            ]\n        )\n        ++ \"|\" ++ yn (isNaN (0.0 / 0.0)) ++ yn (isNaN 1.0)\n        ++ yn (isInfinite (1.0 / 0.0)) ++ yn (isInfinite 2.0)\n        ++ \"|\" ++ String.fromInt (round (pi * 100)) ++ \",\" ++ String.fromInt (round (e * 100))\n",
    );
}

#[test]
fn tuple_param_patterns() {
    // Tuple destructuring in function params (top-level and lambda).
    assert_str_prog(
        "tuple_params",
        "module Test exposing (main)\n\n\
         add : ( Int, Int ) -> Int\n\
         add ( a, b ) = a + b\n\n\
         nested : ( Int, ( String, Int ) ) -> String\n\
         nested ( n, ( s, m ) ) = s ++ String.fromInt (n + m)\n\n\
         main : String\n\
         main =\n    \
            String.fromInt (add ( 3, 4 ))\n        ++ \"|\" ++ nested ( 10, ( \"x\", 5 ) )\n        ++ \"|\" ++ String.join \",\" (List.map (\\( k, v ) -> k ++ String.fromInt v) [ ( \"a\", 1 ), ( \"b\", 2 ) ])\n",
    );
}

#[test]
fn tuple_map_xor_map3() {
    assert_str_prog(
        "tuple_xor_map3",
        "module Test exposing (main)\n\n\
         yn : Bool -> String\n\
         yn b = if b then \"y\" else \"n\"\n\n\
         showT : ( Int, String ) -> String\n\
         showT t =\n    \"(\" ++ String.fromInt (Tuple.first t) ++ \",\" ++ Tuple.second t ++ \")\"\n\n\
         main : String\n\
         main =\n    \
            showT (Tuple.mapFirst (\\n -> n + 1) ( 4, \"x\" ))\n        ++ showT (Tuple.mapSecond (\\s -> s ++ \"!\") ( 4, \"x\" ))\n        ++ showT (Tuple.mapBoth (\\n -> n * 2) (\\s -> String.toUpper s) ( 4, \"x\" ))\n        ++ \"|\" ++ yn (xor True False) ++ yn (xor True True)\n        ++ \"|\" ++ String.join \",\" (List.map3 (\\a b c -> String.fromInt (a + b + c)) [ 1, 2, 3 ] [ 10, 20, 30 ] [ 100, 200 ])\n",
    );
}

#[test]
fn string_pad_and_list_more() {
    assert_str_prog(
        "pad_more",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            \"[\" ++ String.padLeft 5 '0' \"42\" ++ \"]\"\n        ++ \"[\" ++ String.padRight 5 '.' \"ab\" ++ \"]\"\n        ++ \"[\" ++ String.padLeft 2 '0' \"toolong\" ++ \"]\"\n        ++ \"|\" ++ String.join \",\" (List.map String.fromInt (List.concatMap (\\n -> [ n, n ]) [ 1, 2, 3 ]))\n        ++ \"|\" ++ String.join \"\" (List.intersperse \"-\" [ \"a\", \"b\", \"c\" ])\n        ++ \"|\" ++ String.join \"\" (List.intersperse \"-\" [ \"solo\" ])\n",
    );
}

#[test]
fn list_repeat_filtermap_sortby() {
    assert_str_prog(
        "repeat_fm_sortby",
        "module Test exposing (main)\n\n\
         parity : Int -> Maybe Int\n\
         parity n =\n    if modBy 2 n == 0 then Just (n * n) else Nothing\n\n\
         main : String\n\
         main =\n    \
            String.join \",\" (List.map String.fromInt (List.repeat 4 7))\n        ++ \"|\" ++ String.join \",\" (List.map String.fromInt (List.repeat 0 9))\n        ++ \"|\" ++ String.join \",\" (List.map String.fromInt (List.filterMap parity [ 1, 2, 3, 4, 5, 6 ]))\n        ++ \"|\" ++ String.join \",\" (List.sortBy String.length [ \"ccc\", \"a\", \"bb\", \"dddd\" ])\n        ++ \"|\" ++ String.join \",\" (List.map String.fromInt (List.sortBy negate [ 3, 1, 2 ]))\n",
    );
}

#[test]
fn kernels_as_values() {
    // Bare kernels passed to higher-order functions (no lambda wrapper).
    assert_str_prog(
        "kernel_values",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            String.map Char.toUpper \"hello\"\n        ++ \"|\" ++ String.filter Char.isDigit \"a1b2\"\n        ++ \"|\" ++ String.join \",\" (List.map String.toUpper [ \"ab\", \"cd\" ])\n        ++ \"|\" ++ String.join \",\" (List.map String.fromInt (List.map String.length [ \"a\", \"abcd\" ]))\n        ++ \"|\" ++ String.join \",\" (List.map String.reverse [ \"ab\", \"cd\" ])\n",
    );
}

#[test]
fn string_words_lines() {
    assert_str_prog(
        "words_lines",
        "module Test exposing (main)\n\n\
         show : List String -> String\n\
         show xs = \"[\" ++ String.join \"~\" xs ++ \"]\"\n\n\
         main : String\n\
         main =\n    \
            show (String.words \"  the   quick brown\\tfox \")\n        ++ show (String.words \"\")\n        ++ show (String.lines \"a\\nb\\r\\nc\\rd\")\n        ++ show (String.lines \"trailing\\n\")\n",
    );
}

#[test]
fn string_split() {
    assert_str_prog(
        "split",
        "module Test exposing (main)\n\n\
         show : List String -> String\n\
         show xs = \"[\" ++ String.join \"~\" xs ++ \"]\"\n\n\
         main : String\n\
         main =\n    \
            show (String.split \",\" \"a,b,c\")\n        ++ show (String.split \",\" \"a,,b\")\n        ++ show (String.split \",\" \",a,\")\n        ++ show (String.split \",\" \"nocommas\")\n        ++ show (String.split \"\" \"xyz\")\n        ++ show (String.split \", \" \"1, 2, 3\")\n",
    );
}

#[test]
fn result_module() {
    assert_str_prog(
        "result",
        "module Test exposing (main)\n\n\
         parse : String -> Result String Int\n\
         parse s =\n    case String.toInt s of\n        Just n ->\n            Ok n\n\n        Nothing ->\n            Err (\"bad: \" ++ s)\n\n\
         showR : Result String Int -> String\n\
         showR r =\n    case r of\n        Ok n ->\n            \"ok\" ++ String.fromInt n\n\n        Err e ->\n            \"err(\" ++ e ++ \")\"\n\n\
         showM : Maybe Int -> String\n\
         showM m =\n    case m of\n        Just n ->\n            \"j\" ++ String.fromInt n\n\n        Nothing ->\n            \"no\"\n\n\
         main : String\n\
         main =\n    \
            String.fromInt (Result.withDefault 0 (parse \"7\"))\n        ++ \",\" ++ String.fromInt (Result.withDefault 0 (parse \"x\"))\n        ++ \"|\" ++ showR (Result.map (\\n -> n * 2) (parse \"5\"))\n        ++ \",\" ++ showR (Result.map (\\n -> n * 2) (parse \"x\"))\n        ++ \"|\" ++ showR (Result.mapError (\\e -> \"E\") (parse \"x\"))\n        ++ \"|\" ++ showR (Result.andThen (\\n -> Ok (n + 1)) (parse \"9\"))\n        ++ \"|\" ++ showM (Result.toMaybe (parse \"3\"))\n        ++ \",\" ++ showM (Result.toMaybe (parse \"x\"))\n",
    );
}

#[test]
fn basics_clamp() {
    assert_str_prog(
        "clamp",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            String.fromInt (clamp 0 10 5)\n        ++ \",\" ++ String.fromInt (clamp 0 10 -3)\n        ++ \",\" ++ String.fromInt (clamp 0 10 42)\n        ++ \"|\" ++ (if clamp 1.0 2.0 3.5 == 2.0 then \"y\" else \"n\")\n",
    );
}

#[test]
fn string_length_utf16() {
    // Elm String.length counts UTF-16 code units: BMP = 1, astral = 2.
    assert_str_prog(
        "len16",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            String.fromInt (String.length \"abc\")\n        ++ \",\" ++ String.fromInt (String.length \"a\\u{00E9}o\")\n        ++ \",\" ++ String.fromInt (String.length \"\\u{2603}\")\n        ++ \",\" ++ String.fromInt (String.length \"\\u{1F600}\")\n        ++ \",\" ++ String.fromInt (String.length \"\")\n",
    );
}

#[test]
fn string_char_bridge() {
    assert_str_prog(
        "char_bridge",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            String.map (\\c -> Char.toUpper c) \"hello\"\n        ++ \"|\" ++ String.filter (\\c -> Char.isDigit c) \"a1b2c3\"\n        ++ \"|\" ++ String.reverse \"abcde\"\n        ++ \"|\" ++ String.cons 'x' \"yz\"\n        ++ \"|\" ++ String.fromList (String.toList \"roundtrip\")\n        ++ \"|\" ++ String.fromInt (String.foldl (\\c acc -> acc + 1) 0 \"abc\")\n",
    );
}

#[test]
fn string_uncons() {
    assert_str_prog(
        "uncons",
        "module Test exposing (main)\n\n\
         show : Maybe ( Char, String ) -> String\n\
         show m =\n    case m of\n        Just ( c, rest ) ->\n            String.fromChar c ++ \"/\" ++ rest\n\n        Nothing ->\n            \"-\"\n\n\
         main : String\n\
         main =\n    show (String.uncons \"abc\") ++ \"|\" ++ show (String.uncons \"\")\n",
    );
}

#[test]
fn string_utf8_roundtrip() {
    // Multi-byte code points must survive decode/re-encode and reverse.
    assert_str_prog(
        "utf8",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            String.reverse \"a\\u{00E9}o\"\n        ++ \"|\" ++ String.fromList (String.toList \"\\u{2603}\\u{1F600}z\")\n",
    );
}

#[test]
fn char_breadth() {
    assert_str_prog(
        "char_breadth",
        "module Test exposing (main)\n\n\
         yn : Bool -> String\n\
         yn b = if b then \"y\" else \"n\"\n\n\
         cc : Char -> String\n\
         cc c = String.fromInt (Char.toCode c)\n\n\
         main : String\n\
         main =\n    \
            cc (Char.toUpper 'a')\n        ++ \",\" ++ cc (Char.toUpper 'Z')\n        ++ \",\" ++ cc (Char.toUpper '3')\n        ++ \",\" ++ cc (Char.toLower 'A')\n        ++ \",\" ++ cc (Char.toLower 'q')\n        ++ \"|\" ++ yn (Char.isAlpha 'q')\n        ++ yn (Char.isAlpha '7')\n        ++ yn (Char.isAlphaNum '7')\n        ++ yn (Char.isHexDigit 'f')\n        ++ yn (Char.isHexDigit 'g')\n        ++ yn (Char.isOctDigit '7')\n        ++ yn (Char.isOctDigit '8')\n",
    );
}

#[test]
fn string_to_int() {
    assert_str_prog(
        "toint",
        "module Test exposing (main)\n\n\
         show : Maybe Int -> String\n\
         show m =\n    case m of\n        Just n ->\n            String.fromInt n\n\n        Nothing ->\n            \"x\"\n\n\
         main : String\n\
         main =\n    \
            show (String.toInt \"42\")\n        ++ \",\" ++ show (String.toInt \"-17\")\n        ++ \",\" ++ show (String.toInt \"+5\")\n        ++ \",\" ++ show (String.toInt \"12a\")\n        ++ \",\" ++ show (String.toInt \"\")\n        ++ \",\" ++ show (String.toInt \"-\")\n        ++ \",\" ++ show (String.toInt \"007\")\n",
    );
}

#[test]
fn string_contains() {
    assert_str_prog(
        "contains",
        "module Test exposing (main)\n\n\
         yn : Bool -> String\n\
         yn b = if b then \"y\" else \"n\"\n\n\
         main : String\n\
         main =\n    \
            yn (String.contains \"cat\" \"concatenate\")\n        ++ yn (String.contains \"dog\" \"concatenate\")\n        ++ yn (String.contains \"\" \"abc\")\n        ++ yn (String.contains \"abcd\" \"abc\")\n        ++ yn (String.contains \"ate\" \"concatenate\")\n",
    );
}

#[test]
fn string_slicing() {
    assert_str_prog(
        "slicing",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            String.left 3 \"abcdef\"\n        ++ \"|\" ++ String.right 2 \"abcdef\"\n        ++ \"|\" ++ String.dropLeft 2 \"abcdef\"\n        ++ \"|\" ++ String.dropRight 2 \"abcdef\"\n        ++ \"|\" ++ String.left 99 \"ab\"\n        ++ \"|\" ++ String.right 99 \"ab\"\n        ++ \"|\" ++ String.dropLeft 99 \"ab\"\n        ++ \"[\" ++ String.left 0 \"ab\" ++ \"]\"\n",
    );
}

#[test]
fn string_case_trim() {
    assert_str_prog(
        "case_trim",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            String.toUpper \"Hello, World!\"\n        ++ \"|\" ++ String.toLower \"Hello, World!\"\n        ++ \"|\" ++ String.trim \"  \\t spaced \\n \"\n        ++ \"|\" ++ String.trim \"nopad\"\n",
    );
}

#[test]
fn list_map2() {
    assert_str_prog(
        "map2",
        "module Test exposing (main)\n\n\
         main : String\n\
         main =\n    \
            List.map2 (\\a b -> a + b) [ 1, 2, 3 ] [ 10, 20, 30, 40 ]\n        |> List.map String.fromInt\n        |> String.join \",\"\n",
    );
}
