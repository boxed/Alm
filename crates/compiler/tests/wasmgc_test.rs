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
