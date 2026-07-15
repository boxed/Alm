//! WasmGC backend (experimental). Compiles `Test.main` with the WasmGC code
//! generator, runs the module under Node, and checks the result matches the JS
//! backend. Phase W1 covers integer programs only.

mod common;

use std::process::Command;

use alm_compiler::{generate, project};

const RUNNER: &str = r#"
const fs = require('fs');
const bytes = fs.readFileSync(process.argv[2]);
const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {env:new Proxy({},{get:()=>()=>0})});
console.log(instance.exports.main_int().toString());
"#;

// Runs `render()` (for `main : String`), reading the UTF-8 bytes back out of
// the module's linear memory.
const STR_RUNNER: &str = r#"
const fs = require('fs');
const bytes = fs.readFileSync(process.argv[2]);
const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {env:new Proxy({},{get:()=>()=>0})});
const len = instance.exports.render();
const mem = new Uint8Array(instance.exports.memory.buffer, 0, len);
process.stdout.write(Buffer.from(mem).toString('utf8'));
"#;

/// Compile a whole module whose `main : String` and assert all backends agree.
fn assert_str_prog(test_name: &str, source: &str) {
    assert_str_prog_impl(test_name, source, true);
}

/// As `assert_str_prog` but only diffs JS↔WasmGC. Used by the two tests that
/// exercise String edge cases where the native backend has a KNOWN Elm-parity
/// bug (astral `String.length` counts code points not UTF-16 units; native
/// `String.lines` splits only on `\n`, not `\r\n`/`\r`). WasmGC matches JS (the
/// Elm reference) in both; the native gaps are tracked separately.
fn assert_str_prog_js_wasm(test_name: &str, source: &str) {
    assert_str_prog_impl(test_name, source, false);
}

fn assert_str_prog_impl(test_name: &str, source: &str, check_native: bool) {
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

    if check_native {
        if let Some(nat) = native_out(&entry, &dir) {
            assert_eq!(js, nat, "JS and native backends disagree");
        }
    }
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

/// Build the program with the native (typed, vector-List) backend and run it,
/// returning its stdout. Returns `None` when the native toolchain isn't
/// available so the JS/WasmGC diff still runs; a real backend error panics.
/// The binary is run under `timeout` since native code is uncapped.
fn native_out(entry: &std::path::Path, dir: &std::path::Path) -> Option<String> {
    let bin = dir.join("native_app");
    match project::compile_project_typed(entry, &bin, generate::native::Target::Native) {
        Ok(()) => {
            let out = Command::new("timeout")
                .arg("30")
                .arg(&bin)
                .output()
                .expect("run native binary");
            assert!(
                out.status.success(),
                "native run failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        }
        Err(e) => {
            eprintln!(
                "native backend skipped for this test:\n{}",
                e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
            );
            None
        }
    }
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

    if let Some(nat) = native_out(&entry, &dir) {
        assert_eq!(js, nat, "JS and native backends disagree");
    }
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
    // JS↔WasmGC only: native String.lines splits on "\n" alone (misses \r\n/\r).
    assert_str_prog_js_wasm(
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
    // JS↔WasmGC only: native counts code points (astral = 1).
    assert_str_prog_js_wasm(
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

#[test]
fn dict_basics() {
    assert_str_prog(
        "dict_basics",
        "module Test exposing (main)\n\n\
         show : Maybe Int -> String\n\
         show m =\n    case m of\n        Just n ->\n            String.fromInt n\n\n        Nothing ->\n            \"-\"\n\n\
         yn : Bool -> String\n\
         yn b = if b then \"y\" else \"n\"\n\n\
         d : Dict.Dict String Int\n\
         d =\n    Dict.fromList [ ( \"b\", 2 ), ( \"a\", 1 ), ( \"c\", 3 ), ( \"a\", 9 ) ]\n\n\
         main : String\n\
         main =\n    \
            show (Dict.get \"a\" d)\n        ++ \",\" ++ show (Dict.get \"c\" d)\n        ++ \",\" ++ show (Dict.get \"z\" d)\n        ++ \"|\" ++ String.fromInt (Dict.size d)\n        ++ \"|\" ++ yn (Dict.member \"b\" d)\n        ++ yn (Dict.member \"z\" d)\n        ++ \"|\" ++ String.join \",\" (Dict.keys d)\n        ++ \"|\" ++ String.join \",\" (List.map String.fromInt (Dict.values d))\n",
    );
}

#[test]
fn dict_insert_remove_update() {
    assert_str_prog(
        "dict_ins_rem",
        "module Test exposing (main)\n\n\
         showEntry : ( String, Int ) -> String\n\
         showEntry pair = Tuple.first pair ++ String.fromInt (Tuple.second pair)\n\n\
         dump : Dict.Dict String Int -> String\n\
         dump dd = String.join \",\" (List.map showEntry (Dict.toList dd))\n\n\
         main : String\n\
         main =\n    \
            let\n        d0 = Dict.empty\n        d1 = Dict.insert \"m\" 5 (Dict.insert \"a\" 1 (Dict.insert \"z\" 9 d0))\n        d2 = Dict.insert \"a\" 100 d1\n        d3 = Dict.remove \"z\" d2\n        d4 = Dict.update \"m\" (\\mv -> Maybe.map (\\v -> v + 1) mv) d3\n        d5 = Dict.update \"x\" (\\_ -> Just 7) d4\n    in\n    dump d1 ++ \"|\" ++ dump d2 ++ \"|\" ++ dump d3 ++ \"|\" ++ dump d4 ++ \"|\" ++ dump d5\n",
    );
}

#[test]
fn dict_fold_map_filter_combine() {
    assert_str_prog(
        "dict_combine",
        "module Test exposing (main)\n\n\
         showEntry : ( String, Int ) -> String\n\
         showEntry pair = Tuple.first pair ++ String.fromInt (Tuple.second pair)\n\n\
         dump : Dict.Dict String Int -> String\n\
         dump dd = String.join \",\" (List.map showEntry (Dict.toList dd))\n\n\
         a : Dict.Dict String Int\n\
         a = Dict.fromList [ ( \"a\", 1 ), ( \"b\", 2 ), ( \"c\", 3 ) ]\n\n\
         b : Dict.Dict String Int\n\
         b = Dict.fromList [ ( \"b\", 20 ), ( \"d\", 40 ) ]\n\n\
         main : String\n\
         main =\n    \
            String.fromInt (Dict.foldl (\\_ v acc -> v + acc) 0 a)\n        ++ \"|\" ++ dump (Dict.map (\\_ v -> v * 10) a)\n        ++ \"|\" ++ dump (Dict.filter (\\_ v -> v > 1) a)\n        ++ \"|\" ++ dump (Dict.union a b)\n        ++ \"|\" ++ dump (Dict.intersect a b)\n        ++ \"|\" ++ dump (Dict.diff a b)\n",
    );
}

#[test]
fn set_basics() {
    assert_str_prog(
        "set_basics",
        "module Test exposing (main)\n\n\
         yn : Bool -> String\n\
         yn b = if b then \"y\" else \"n\"\n\n\
         dump : Set.Set Int -> String\n\
         dump s = String.join \",\" (List.map String.fromInt (Set.toList s))\n\n\
         s : Set.Set Int\n\
         s = Set.fromList [ 3, 1, 2, 3, 1 ]\n\n\
         main : String\n\
         main =\n    \
            dump s\n        ++ \"|\" ++ String.fromInt (Set.size s)\n        ++ \"|\" ++ yn (Set.member 2 s)\n        ++ yn (Set.member 9 s)\n        ++ \"|\" ++ dump (Set.insert 5 (Set.remove 1 s))\n        ++ \"|\" ++ String.fromInt (Set.foldl (+) 0 s)\n",
    );
}

#[test]
fn set_combine() {
    assert_str_prog(
        "set_combine",
        "module Test exposing (main)\n\n\
         dump : Set.Set Int -> String\n\
         dump s = String.join \",\" (List.map String.fromInt (Set.toList s))\n\n\
         a : Set.Set Int\n\
         a = Set.fromList [ 1, 2, 3, 4 ]\n\n\
         b : Set.Set Int\n\
         b = Set.fromList [ 3, 4, 5, 6 ]\n\n\
         main : String\n\
         main =\n    \
            dump (Set.union a b)\n        ++ \"|\" ++ dump (Set.intersect a b)\n        ++ \"|\" ++ dump (Set.diff a b)\n        ++ \"|\" ++ dump (Set.filter (\\x -> modBy 2 x == 0) a)\n        ++ \"|\" ++ dump (Set.map (\\x -> x * x) a)\n",
    );
}

#[test]
fn array_basics() {
    assert_str_prog(
        "array_basics",
        "module Test exposing (main)\n\n\
         show : Maybe Int -> String\n\
         show m =\n    case m of\n        Just n ->\n            String.fromInt n\n\n        Nothing ->\n            \"-\"\n\n\
         dump : Array.Array Int -> String\n\
         dump a = String.join \",\" (List.map String.fromInt (Array.toList a))\n\n\
         arr : Array.Array Int\n\
         arr = Array.fromList [ 10, 20, 30, 40 ]\n\n\
         main : String\n\
         main =\n    \
            String.fromInt (Array.length arr)\n        ++ \"|\" ++ show (Array.get 2 arr)\n        ++ \",\" ++ show (Array.get 9 arr)\n        ++ \"|\" ++ dump (Array.set 1 99 arr)\n        ++ \"|\" ++ dump (Array.push 50 arr)\n        ++ \"|\" ++ dump (Array.initialize 4 (\\i -> i * i))\n        ++ \"|\" ++ dump (Array.slice 1 3 arr)\n        ++ \"|\" ++ dump (Array.slice 1 -1 arr)\n        ++ \"|\" ++ String.fromInt (Array.foldl (+) 0 arr)\n        ++ \"|\" ++ dump (Array.map (\\x -> x + 1) arr)\n        ++ \"|\" ++ dump (Array.filter (\\x -> x > 15) arr)\n",
    );
}

#[test]
fn array_indexed() {
    assert_str_prog(
        "array_indexed",
        "module Test exposing (main)\n\n\
         showPair : ( Int, Int ) -> String\n\
         showPair p = String.fromInt (Tuple.first p) ++ \":\" ++ String.fromInt (Tuple.second p)\n\n\
         arr : Array.Array Int\n\
         arr = Array.fromList [ 7, 8, 9 ]\n\n\
         main : String\n\
         main =\n    \
            String.join \",\" (List.map showPair (Array.toIndexedList arr))\n        ++ \"|\" ++ String.join \",\" (List.map String.fromInt (Array.toList (Array.indexedMap (\\i x -> i + x) arr)))\n",
    );
}

#[test]
fn json_encode_compact() {
    assert_str_prog(
        "json_compact",
        "module Test exposing (main)\n\n\
         import Json.Encode as E\n\n\
         main : String\n\
         main =\n    \
            E.encode 0\n        (E.object\n            [ ( \"name\", E.string \"Ann \\\"Q\\\"\" )\n            , ( \"age\", E.int 30 )\n            , ( \"ratio\", E.float 2.0 )\n            , ( \"tags\", E.list E.string [ \"a\", \"b\" ] )\n            , ( \"active\", E.bool True )\n            , ( \"note\", E.null )\n            , ( \"empty\", E.object [] )\n            ]\n        )\n",
    );
}

#[test]
fn json_encode_pretty() {
    assert_str_prog(
        "json_pretty",
        "module Test exposing (main)\n\n\
         import Json.Encode as E\n\n\
         main : String\n\
         main =\n    \
            E.encode 2\n        (E.object\n            [ ( \"a\", E.int 1 )\n            , ( \"nested\", E.list E.int [ 1, 2, 3 ] )\n            ]\n        )\n",
    );
}

#[test]
fn json_decode_basics() {
    assert_str_prog(
        "json_dec",
        "module Test exposing (main)\n\n\
         import Json.Decode as D\n\n\
         person : D.Decoder String\n\
         person =\n    D.map2 (\\n a -> n ++ \"/\" ++ String.fromInt a)\n        (D.field \"name\" D.string)\n        (D.field \"age\" D.int)\n\n\
         main : String\n\
         main =\n    \
            (case D.decodeString person \"{ \\\"name\\\": \\\"Bob\\\", \\\"age\\\": 42 }\" of\n        Ok s -> s\n\n        Err _ -> \"ERR\"\n    )\n        ++ \"|\" ++ (case D.decodeString (D.list D.int) \"[1, 2, 3]\" of\n        Ok xs -> String.join \",\" (List.map String.fromInt xs)\n\n        Err _ -> \"ERR\"\n    )\n        ++ \"|\" ++ (case D.decodeString (D.field \"a\" (D.field \"b\" D.bool)) \"{\\\"a\\\":{\\\"b\\\":true}}\" of\n        Ok b -> if b then \"y\" else \"n\"\n\n        Err _ -> \"ERR\"\n    )\n        ++ \"|\" ++ (case D.decodeString D.int \"not json\" of\n        Ok _ -> \"ok\"\n\n        Err _ -> \"ERR\"\n    )\n",
    );
}

#[test]
fn json_decode_combinators() {
    assert_str_prog(
        "json_dec2",
        "module Test exposing (main)\n\n\
         import Json.Decode as D\n\n\
         showMaybe : Maybe Int -> String\n\
         showMaybe m =\n    case m of\n        Just n -> String.fromInt n\n\n        Nothing -> \"-\"\n\n\
         main : String\n\
         main =\n    \
            (case D.decodeString (D.maybe (D.field \"x\" D.int)) \"{\\\"y\\\":1}\" of\n        Ok m -> showMaybe m\n\n        Err _ -> \"ERR\"\n    )\n        ++ \"|\" ++ (case D.decodeString (D.oneOf [ D.int, D.succeed 0 ]) \"\\\"hi\\\"\" of\n        Ok n -> String.fromInt n\n\n        Err _ -> \"ERR\"\n    )\n        ++ \"|\" ++ (case D.decodeString (D.index 1 D.string) \"[\\\"a\\\",\\\"b\\\"]\" of\n        Ok s -> s\n\n        Err _ -> \"ERR\"\n    )\n        ++ \"|\" ++ (case D.decodeString (D.at [ \"a\", \"b\" ] D.int) \"{\\\"a\\\":{\\\"b\\\":7}}\" of\n        Ok n -> String.fromInt n\n\n        Err _ -> \"ERR\"\n    )\n        ++ \"|\" ++ (case D.decodeString (D.nullable D.int) \"null\" of\n        Ok m -> showMaybe m\n\n        Err _ -> \"ERR\"\n    )\n",
    );
}

/// Browser.sandbox static-render parity: the WasmGC `render_html` export vs the
/// JS backend rendering the same program into the shared DOM stub.
fn assert_sandbox_html(test_name: &str, source: &str) {
    let dir = common::test_dir("alm-wasmgc", test_name);
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });

    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");

    // JS oracle: render the program into the DOM stub, serialize the body.
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let oracle = dir.join("oracle.cjs");
    std::fs::write(
        &oracle,
        format!(
            "const {{Document,serializeBody}}=require({sup:?}+'/dom_stub.cjs');\
             const {{start}}=require({sup:?}+'/js_driver.cjs');\
             const doc=new Document();start({b:?},doc);\
             process.stdout.write(serializeBody(doc));",
            sup = support, b = bundle.display()
        ),
    )
    .expect("oracle");
    let js = run(Command::new("node").arg(&oracle));

    // WasmGC: call render_html and read the string out of linear memory.
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let runner = dir.join("run_html.cjs");
    std::fs::write(
        &runner,
        "const fs=require('fs');const b=fs.readFileSync(process.argv[2]);\
         const i=new WebAssembly.Instance(new WebAssembly.Module(b),{env:new Proxy({},{get:()=>()=>0})});\
         const n=i.exports.render_html();\
         process.stdout.write(Buffer.from(new Uint8Array(i.exports.memory.buffer,0,n)).toString('utf8'));",
    )
    .expect("runner");
    let wasm_out = run(Command::new("node").arg(&runner).arg(&wasm));

    assert_eq!(js, wasm_out, "JS and WasmGC sandbox render disagree");
}

#[test]
fn sandbox_static_render() {
    assert_sandbox_html(
        "sandbox",
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (div, span, text)\n\
         import Html.Attributes exposing (attribute, style)\n\n\
         type Msg = Noop\n\n\
         update : Msg -> Int -> Int\n\
         update _ m = m\n\n\
         view : Int -> Html.Html Msg\n\
         view _ =\n    \
            div [ attribute \"id\" \"root\" ]\n        [ span [] [ text \"hi <b> & x\" ]\n        , div [ style \"color\" \"red\", attribute \"data-n\" \"1\" ] [ text \"y\" ]\n        , text \"tail\"\n        ]\n\n\
         main : Program () Int Msg\n\
         main = Browser.sandbox { init = 0, update = update, view = view }\n",
    );
}

/// Browser.sandbox event parity: initial render + after a click, WasmGC (real
/// DOM via host imports) vs the JS backend, both driven through the DOM stub.
fn assert_sandbox_click(test_name: &str, source: &str) {
    let dir = common::test_dir("alm-wasmgc", test_name);
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");

    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });

    let script = dir.join("m2.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function findBtn(n){{if(n.tagName==='button')return n;for(const c of (n.childNodes||[])){{const r=findBtn(c);if(r)return r;}}return null;}}\
             function run(startFn,arg){{const doc=new Document();startFn(arg,doc);\
               const a=serializeBody(doc);const b=findBtn(doc.body);if(b)dispatchEvent(b,'click',{{}});\
               const c=serializeBody(doc);return [a,c];}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write([j[0],w[0],j[1],w[1]].join('\\u001e'));",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 4, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "initial render disagrees");
    assert_eq!(parts[2], parts[3], "post-click render disagrees");
}

#[test]
fn sandbox_click_counter() {
    assert_sandbox_click(
        "sandbox_click",
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (button, div, text)\n\
         import Html.Events exposing (onClick)\n\n\
         type Msg = Inc\n\n\
         update : Msg -> Int -> Int\n\
         update _ n = n + 1\n\n\
         view : Int -> Html.Html Msg\n\
         view n =\n    div [] [ button [ onClick Inc ] [ text \"+\" ], div [] [ text (String.fromInt n) ] ]\n\n\
         main : Program () Int Msg\n\
         main = Browser.sandbox { init = 0, update = update, view = view }\n",
    );
}

#[test]
fn element_incoming_port() {
    // An incoming port subscription: send a value from the host, decode it in
    // update, and assert both backends render the delivered value identically.
    let dir = common::test_dir("alm-wasmgc", "incoming_port");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "port module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (div, text)\n\
         import Json.Decode as D\n\n\
         port fromJs : (D.Value -> msg) -> Sub msg\n\n\
         type Msg = Got D.Value\n\n\
         init : () -> ( Int, Cmd Msg )\n\
         init _ = ( 0, Cmd.none )\n\n\
         update : Msg -> Int -> ( Int, Cmd Msg )\n\
         update (Got v) _ =\n    ( case D.decodeValue D.int v of\n        Ok n -> n\n        Err _ -> -1\n    , Cmd.none )\n\n\
         view : Int -> Html.Html Msg\n\
         view n =\n    div [] [ text (String.fromInt n) ]\n\n\
         main : Program () Int Msg\n\
         main =\n    Browser.element { init = init, update = update, view = view, subscriptions = \\_ -> fromJs Got }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m7.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function run(startFn,arg){{const doc=new Document();const r=startFn(arg,doc);\
               r.sendPort('fromJs',42);return serializeBody(doc);}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "incoming-port render disagrees");
    assert_eq!(parts[1], "<div>42</div>", "expected delivered value 42");
}

#[test]
fn document_title_and_body() {
    // Browser.document: view returns { title, body }. Assert both backends set
    // the same title and render the same body, at init and after a click.
    let dir = common::test_dir("alm-wasmgc", "document");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (button, div, text)\n\
         import Html.Events exposing (onClick)\n\n\
         type Msg = Inc\n\n\
         init : () -> ( Int, Cmd Msg )\n\
         init _ = ( 0, Cmd.none )\n\n\
         update : Msg -> Int -> ( Int, Cmd Msg )\n\
         update _ n = ( n + 1, Cmd.none )\n\n\
         view : Int -> Browser.Document Msg\n\
         view n =\n    { title = \"Count \" ++ String.fromInt n\n    , body = [ button [ onClick Inc ] [ text \"+\" ], div [] [ text (String.fromInt n) ] ]\n    }\n\n\
         main : Program () Int Msg\n\
         main =\n    Browser.document { init = init, update = update, view = view, subscriptions = \\_ -> Sub.none }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m6.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function findBtn(n){{if(n.tagName==='button')return n;for(const c of (n.childNodes||[])){{const r=findBtn(c);if(r)return r;}}return null;}}\
             function run(startFn,arg){{const doc=new Document();startFn(arg,doc);\
               const a=doc.title+'|'+serializeBody(doc);\
               const b=findBtn(doc.body);if(b)dispatchEvent(b,'click',{{}});\
               const c=doc.title+'|'+serializeBody(doc);return [a,c];}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write([j[0],w[0],j[1],w[1]].join('\\u001e'));",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 4, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "initial title/body disagree");
    assert_eq!(parts[2], parts[3], "post-click title/body disagree");
    assert_eq!(
        parts[2], "Count 1|<div><button>+</button><div>1</div></div>",
        "unexpected document render"
    );
}

#[test]
fn accessor_function() {
    // `.field` used as a first-class function (List.map .name / .age).
    assert_str_prog(
        "accessor",
        "module Test exposing (main)\n\n\
         people : List { name : String, age : Int }\n\
         people = [ { name = \"ann\", age = 3 }, { name = \"bo\", age = 1 } ]\n\n\
         main : String\n\
         main =\n    String.join \",\" (List.map .name people)\n\
         \x20       ++ \"|\"\n\
         \x20       ++ String.fromInt (List.sum (List.map .age people))\n",
    );
}

#[test]
fn url_from_string_pure() {
    // De-risk the hand-written Url.fromString parser as a pure function, diffed
    // across all three backends, before wiring Browser.application.
    assert_str_prog(
        "url_from_string",
        "module Test exposing (main)\n\n\
         import Url\n\n\
         one : String -> String\n\
         one s =\n    case Url.fromString s of\n\
         \x20       Nothing -> \"NOTHING\"\n\
         \x20       Just u ->\n\
         \x20           u.host ++ \"|\" ++ String.fromInt (Maybe.withDefault 0 u.port_)\n\
         \x20               ++ \"|\" ++ u.path\n\
         \x20               ++ \"|\" ++ Maybe.withDefault \"-\" u.query\n\
         \x20               ++ \"|\" ++ Maybe.withDefault \"-\" u.fragment\n\n\
         main : String\n\
         main =\n    String.join \"\\n\"\n\
         \x20       [ one \"https://example.com:8080/a/b?x=1#frag\"\n\
         \x20       , one \"http://elm-lang.org/\"\n\
         \x20       , one \"https://foo.com\"\n\
         \x20       , one \"ftp://nope.com/\"\n\
         \x20       , one \"https://a.com/p?q\"\n\
         \x20       , one \"https://a.com/p#f\"\n\
         \x20       ]\n",
    );
}

#[test]
fn element_animation_frame() {
    // onAnimationFrameDelta: advance the clock and flush one frame; the delta
    // (rounded to avoid the float→string gap) must render the same in both.
    let dir = common::test_dir("alm-wasmgc", "anim_frame");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Browser.Events\n\
         import Html exposing (div, text)\n\n\
         type Msg = Tick Float\n\n\
         init : () -> ( Int, Cmd Msg )\n\
         init _ = ( 0, Cmd.none )\n\n\
         update : Msg -> Int -> ( Int, Cmd Msg )\n\
         update (Tick d) n = ( n + round d, Cmd.none )\n\n\
         view : Int -> Html.Html Msg\n\
         view n =\n    div [] [ text (String.fromInt n) ]\n\n\
         main : Program () Int Msg\n\
         main =\n    Browser.element { init = init, update = update, view = view, subscriptions = \\_ -> Browser.Events.onAnimationFrameDelta Tick }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m14.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function run(startFn,arg){{const doc=new Document();const r=startFn(arg,doc);\
               r.clock.advance(16);r.clock.flushFrame();\
               const out=serializeBody(doc);if(r.restore)r.restore();return out;}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "onAnimationFrameDelta render disagrees");
    assert_eq!(parts[1], "<div>16</div>", "expected one 16ms frame");
}

#[test]
fn sandbox_keyed_and_lazy() {
    // Html.Keyed.ul with keyed <li>s and Html.Lazy.lazy wrapping a view. Output
    // must match the JS backend at init and after a click that reorders.
    let dir = common::test_dir("alm-wasmgc", "keyed_lazy");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (Html, button, div, li, text)\n\
         import Html.Attributes exposing (id)\n\
         import Html.Events exposing (onClick)\n\
         import Html.Keyed as Keyed\n\
         import Html.Lazy exposing (lazy)\n\n\
         type Msg = Flip\n\n\
         row : Int -> ( String, Html Msg )\n\
         row n = ( String.fromInt n, li [ id (String.fromInt n) ] [ text (String.fromInt n) ] )\n\n\
         listView : Bool -> Html Msg\n\
         listView flipped =\n    Keyed.ul []\n\
         \x20       (if flipped then [ row 2, row 1 ] else [ row 1, row 2 ])\n\n\
         update : Msg -> Bool -> Bool\n\
         update _ b = not b\n\n\
         view : Bool -> Html Msg\n\
         view b =\n    div [] [ button [ onClick Flip ] [ text \"f\" ], lazy listView b ]\n\n\
         main : Program () Bool Msg\n\
         main = Browser.sandbox { init = False, update = update, view = view }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m13.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function findBtn(n){{if(n.tagName==='button')return n;for(const c of (n.childNodes||[])){{const r=findBtn(c);if(r)return r;}}return null;}}\
             function run(startFn,arg){{const doc=new Document();startFn(arg,doc);\
               const a=serializeBody(doc);dispatchEvent(findBtn(doc.body),'click',{{}});\
               const b=serializeBody(doc);return a+' >> '+b;}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "keyed/lazy render disagrees");
}

#[test]
fn application_nav() {
    // Browser.application: init parses the location into the model; clicking
    // pushes a new URL, whose onUrlChange updates the model. Assert both
    // backends render the initial path then the navigated path identically.
    let dir = common::test_dir("alm-wasmgc", "application");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Browser.Navigation as Nav\n\
         import Html exposing (button, div, text)\n\
         import Html.Events exposing (onClick)\n\
         import Url\n\n\
         type alias Model = { key : Nav.Key, path : String }\n\n\
         type Msg = Go | Changed Url.Url\n\n\
         init : () -> Url.Url -> Nav.Key -> ( Model, Cmd Msg )\n\
         init _ url key = ( { key = key, path = url.path }, Cmd.none )\n\n\
         update : Msg -> Model -> ( Model, Cmd Msg )\n\
         update msg model =\n    case msg of\n\
         \x20       Go -> ( model, Nav.pushUrl model.key \"/page2\" )\n\
         \x20       Changed url -> ( { model | path = url.path }, Cmd.none )\n\n\
         view : Model -> Browser.Document Msg\n\
         view model =\n    { title = \"t\", body = [ button [ onClick Go ] [ text \"go\" ], div [] [ text model.path ] ] }\n\n\
         main : Program () Model Msg\n\
         main =\n    Browser.application\n\
         \x20       { init = init, update = update, view = view\n\
         \x20       , subscriptions = \\_ -> Sub.none\n\
         \x20       , onUrlChange = Changed, onUrlRequest = \\_ -> Go\n\
         \x20       }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m12.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function findBtn(n){{if(n.tagName==='button')return n;for(const c of (n.childNodes||[])){{const r=findBtn(c);if(r)return r;}}return null;}}\
             function run(startFn,arg){{const doc=new Document();const r=startFn(arg,doc);\
               const a=serializeBody(doc);dispatchEvent(findBtn(doc.body),'click',{{}});\
               const b=serializeBody(doc);if(r.restore)r.restore();return a+' >> '+b;}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "application render disagrees");
    assert!(parts[1].contains("/page2"), "expected navigation to /page2, got {}", parts[1]);
}

#[test]
fn element_browser_events_keydown() {
    // Browser.Events.onKeyDown with a decoder reading the "key" field. Firing a
    // document keydown must decode and render the key identically in both.
    let dir = common::test_dir("alm-wasmgc", "browser_events");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Browser.Events\n\
         import Html exposing (div, text)\n\
         import Json.Decode as D\n\n\
         type Msg = Key String\n\n\
         init : () -> ( String, Cmd Msg )\n\
         init _ = ( \"none\", Cmd.none )\n\n\
         update : Msg -> String -> ( String, Cmd Msg )\n\
         update (Key k) _ = ( k, Cmd.none )\n\n\
         view : String -> Html.Html Msg\n\
         view s =\n    div [] [ text s ]\n\n\
         subs : String -> Sub Msg\n\
         subs _ =\n    Browser.Events.onKeyDown (D.map Key (D.field \"key\" D.string))\n\n\
         main : Program () String Msg\n\
         main =\n    Browser.element { init = init, update = update, view = view, subscriptions = subs }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m11.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchDocEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function run(startFn,arg){{const doc=new Document();const r=startFn(arg,doc);\
               dispatchDocEvent(doc,'keydown',{{key:'x'}});\
               const out=serializeBody(doc);if(r.restore)r.restore();return out;}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "onKeyDown render disagrees");
    assert_eq!(parts[1], "<div>x</div>", "expected decoded key 'x'");
}

#[test]
fn element_time_every() {
    // Time.every subscription. Advancing the (shared, deterministic) virtual
    // clock past two intervals must leave both backends showing the last tick's
    // posix millis, identically.
    let dir = common::test_dir("alm-wasmgc", "time_every");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (div, text)\n\
         import Time\n\n\
         type Msg = Tick Time.Posix\n\n\
         init : () -> ( Int, Cmd Msg )\n\
         init _ = ( 0, Cmd.none )\n\n\
         update : Msg -> Int -> ( Int, Cmd Msg )\n\
         update (Tick p) _ = ( Time.posixToMillis p, Cmd.none )\n\n\
         view : Int -> Html.Html Msg\n\
         view n =\n    div [] [ text (String.fromInt n) ]\n\n\
         main : Program () Int Msg\n\
         main =\n    Browser.element { init = init, update = update, view = view, subscriptions = \\_ -> Time.every 1000 Tick }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m10.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function run(startFn,arg){{const doc=new Document();const r=startFn(arg,doc);\
               r.clock.advance(2500);const out=serializeBody(doc);if(r.restore)r.restore();return out;}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "Time.every render disagrees");
    assert_eq!(parts[1], "<div>2000</div>", "expected last tick at t=2000");
}

#[test]
fn element_http_get() {
    // Browser.element issues an Http.get on click; the host settles it with a
    // 200 body then a 404. Assert both backends render the same Result each time.
    let dir = common::test_dir("alm-wasmgc", "http_get");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (button, div, text)\n\
         import Html.Events exposing (onClick)\n\
         import Http\n\n\
         type Msg = Fetch | Got (Result Http.Error String)\n\n\
         init : () -> ( String, Cmd Msg )\n\
         init _ = ( \"start\", Cmd.none )\n\n\
         update : Msg -> String -> ( String, Cmd Msg )\n\
         update msg model =\n    case msg of\n        Fetch ->\n            ( model, Http.get { url = \"/data\", expect = Http.expectString Got } )\n        Got (Ok s) ->\n            ( \"ok:\" ++ s, Cmd.none )\n        Got (Err (Http.BadStatus code)) ->\n            ( \"bad:\" ++ String.fromInt code, Cmd.none )\n        Got (Err _) ->\n            ( \"err\", Cmd.none )\n\n\
         view : String -> Html.Html Msg\n\
         view s =\n    div [] [ button [ onClick Fetch ] [ text \"go\" ], div [] [ text s ] ]\n\n\
         main : Program () String Msg\n\
         main =\n    Browser.element { init = init, update = update, view = view, subscriptions = \\_ -> Sub.none }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m9.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function findBtn(n){{if(n.tagName==='button')return n;for(const c of (n.childNodes||[])){{const r=findBtn(c);if(r)return r;}}return null;}}\
             const tick=()=>new Promise(r=>setImmediate(r));\
             async function run(startFn,arg,status,body){{const doc=new Document();const r=startFn(arg,doc);\
               dispatchEvent(findBtn(doc.body),'click',{{}});await tick();\
               r.resolveHttp(status,body);await tick();await tick();\
               const out=serializeBody(doc);if(r.restore)r.restore();return out;}}\
             (async()=>{{\
               const j1=await run(js.start,{b:?},200,'hello');const w1=await run(wg.start,{w:?},200,'hello');\
               const j2=await run(js.start,{b:?},404,'nope');const w2=await run(wg.start,{w:?},404,'nope');\
               process.stdout.write([j1,w1,j2,w2].join('\\u001e'));\
             }})();",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 4, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "200 render disagrees");
    assert_eq!(parts[2], parts[3], "404 render disagrees");
    assert_eq!(parts[1], "<div><button>go</button><div>ok:hello</div></div>", "200 body");
    assert_eq!(parts[3], "<div><button>go</button><div>bad:404</div></div>", "404 status");
}

#[test]
fn sandbox_html_map() {
    // Html.map wraps a child view's messages. Clicking the mapped button must
    // route through the outer Msg (Wrap Bump) and increment — identical in both
    // backends, at init and after a click.
    let dir = common::test_dir("alm-wasmgc", "html_map");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (Html, button, div, text)\n\
         import Html.Events exposing (onClick)\n\n\
         type Child = Bump\n\n\
         childView : Html Child\n\
         childView =\n    button [ onClick Bump ] [ text \"+\" ]\n\n\
         type Msg = Wrap Child\n\n\
         update : Msg -> Int -> Int\n\
         update _ n = n + 1\n\n\
         view : Int -> Html Msg\n\
         view n =\n    div [] [ Html.map Wrap childView, div [] [ text (String.fromInt n) ] ]\n\n\
         main : Program () Int Msg\n\
         main = Browser.sandbox { init = 0, update = update, view = view }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m8.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function findBtn(n){{if(n.tagName==='button')return n;for(const c of (n.childNodes||[])){{const r=findBtn(c);if(r)return r;}}return null;}}\
             function run(startFn,arg){{const doc=new Document();startFn(arg,doc);\
               const b=findBtn(doc.body);if(b)dispatchEvent(b,'click',{{}});\
               return serializeBody(doc);}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "Html.map render disagrees");
    assert_eq!(parts[1], "<div><button>+</button><div>1</div></div>", "mapped click should increment");
}

#[test]
fn sandbox_on_input() {
    // onInput carries the event payload (target.value). Type into the field and
    // assert both backends render the echoed text identically.
    let dir = common::test_dir("alm-wasmgc", "on_input");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (div, input, text)\n\
         import Html.Events exposing (onInput)\n\n\
         type Msg = Typed String\n\n\
         update : Msg -> String -> String\n\
         update (Typed s) _ = s\n\n\
         view : String -> Html.Html Msg\n\
         view s =\n    div [] [ input [ onInput Typed ] [], div [] [ text s ] ]\n\n\
         main : Program () String Msg\n\
         main = Browser.sandbox { init = \"\", update = update, view = view }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m5.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function findInput(n){{if(n.tagName==='input')return n;for(const c of (n.childNodes||[])){{const r=findInput(c);if(r)return r;}}return null;}}\
             function run(startFn,arg){{const doc=new Document();startFn(arg,doc);\
               const i=findInput(doc.body);dispatchEvent(i,'input',{{target:{{value:'héllo'}}}});\
               return serializeBody(doc);}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "onInput render disagrees");
    assert!(parts[1].contains("héllo"), "expected echoed input, got {}", parts[1]);
}

#[test]
fn element_outgoing_port() {
    // Browser.element whose update returns a Cmd that sends an outgoing port.
    // Assert the WasmGC backend produces the same outgoing JSON as the JS one,
    // both at init and after a click.
    let dir = common::test_dir("alm-wasmgc", "element_port");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "port module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (button, div, text)\n\
         import Html.Events exposing (onClick)\n\
         import Json.Encode as E\n\n\
         port out : E.Value -> Cmd msg\n\n\
         type Msg = Inc\n\n\
         init : () -> ( Int, Cmd Msg )\n\
         init _ = ( 0, Cmd.none )\n\n\
         update : Msg -> Int -> ( Int, Cmd Msg )\n\
         update _ n = ( n + 1, out (E.int (n + 1)) )\n\n\
         view : Int -> Html.Html Msg\n\
         view n =\n    div [] [ button [ onClick Inc ] [ text \"+\" ], div [] [ text (String.fromInt n) ] ]\n\n\
         main : Program () Int Msg\n\
         main =\n    Browser.element { init = init, update = update, view = view, subscriptions = \\_ -> Sub.none }\n",
    )
    .expect("fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("bundle");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m4.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const js=require(S+'/js_driver.cjs');const wg=require(S+'/wasmgc_driver.cjs');\
             function findBtn(n){{if(n.tagName==='button')return n;for(const c of (n.childNodes||[])){{const r=findBtn(c);if(r)return r;}}return null;}}\
             function run(startFn,arg){{const doc=new Document();const r=startFn(arg,doc);\
               const b=findBtn(doc.body);if(b)dispatchEvent(b,'click',{{}});\
               return (r.outgoing.out||[]).join(',');}}\
             const j=run(js.start,{b:?});const w=run(wg.start,{w:?});\
             process.stdout.write(j+'\\u001e'+w);",
            sup = support, b = bundle.display(), w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    let parts: Vec<&str> = out.split('\u{1e}').collect();
    assert_eq!(parts.len(), 2, "unexpected output: {out}");
    assert_eq!(parts[0], parts[1], "outgoing port stream disagrees");
    assert_eq!(parts[0], "1", "expected click to send out (E.int 1)");
}

#[test]
fn sandbox_diff_preserves_identity() {
    // After a click that only changes a text node, the diff/patch must keep the
    // unchanged <button> DOM node (a full rebuild would replace it).
    let dir = common::test_dir("alm-wasmgc", "sandbox_diff");
    let entry = dir.join("Test.elm");
    std::fs::write(
        &entry,
        "module Test exposing (main)\n\n\
         import Browser\n\
         import Html exposing (button, div, text)\n\
         import Html.Events exposing (onClick)\n\n\
         type Msg = Inc\n\n\
         update : Msg -> Int -> Int\n\
         update _ n = n + 1\n\n\
         view : Int -> Html.Html Msg\n\
         view n =\n    div [] [ button [ onClick Inc ] [ text \"+\" ], div [] [ text (String.fromInt n) ] ]\n\n\
         main : Program () Int Msg\n\
         main = Browser.sandbox { init = 0, update = update, view = view }\n",
    )
    .expect("fixture");
    project::check_project(&entry).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let script = dir.join("m3.cjs");
    std::fs::write(
        &script,
        format!(
            "const S={sup:?};\
             const {{Document,serializeBody,dispatchEvent}}=require(S+'/dom_stub.cjs');\
             const wg=require(S+'/wasmgc_driver.cjs');\
             function findBtn(n){{if(n.tagName==='button')return n;for(const c of (n.childNodes||[])){{const r=findBtn(c);if(r)return r;}}return null;}}\
             const doc=new Document();wg.start({w:?},doc);\
             const b0=findBtn(doc.body);dispatchEvent(b0,'click',{{}});const b1=findBtn(doc.body);\
             process.stdout.write((b0===b1?'SAME':'NEW')+'|'+serializeBody(doc));",
            sup = support, w = wasm.display()
        ),
    )
    .expect("script");
    let out = run(Command::new("node").arg(&script));
    assert_eq!(
        out, "SAME|<div><button>+</button><div>1</div></div>",
        "diff/patch should preserve the unchanged button node and update the count"
    );
}

/// Rough JS-vs-WasmGC compute benchmark across workload classes. Ignored in CI
/// (perf, not correctness); run with:
///   cargo test -p alm-compiler --test wasmgc_test bench_wasmgc_vs_js -- --ignored --nocapture
#[test]
#[ignore]
fn bench_wasmgc_vs_js() {
    use std::time::Instant;
    // (name, heavy main body, base main body). Each `main : String`; timing the
    // whole node process and subtracting the base cancels startup/instantiate.
    let prelude = "module Test exposing (main)\n\n";
    let workloads: &[(&str, &str, &str)] = &[
        ("fib(33) calls+int",
         "fib : Int -> Int\nfib n = if n < 2 then n else fib (n-1) + fib (n-2)\nmain : String\nmain = String.fromInt (fib 33)\n",
         "main : String\nmain = \"0\"\n"),
        ("foldl sum 3M int",
         "main : String\nmain = String.fromInt (List.foldl (+) 0 (List.range 1 3000000))\n",
         "main : String\nmain = String.fromInt (List.foldl (+) 0 (List.range 1 1))\n"),
        ("map+length 1M alloc",
         "main : String\nmain = String.fromInt (List.length (List.map (\\x -> x * 2) (List.range 1 1000000)))\n",
         "main : String\nmain = String.fromInt (List.length (List.map (\\x -> x * 2) (List.range 1 1)))\n"),
        ("string join 200k",
         "main : String\nmain = String.fromInt (String.length (String.join \",\" (List.map String.fromInt (List.range 1 200000))))\n",
         "main : String\nmain = String.fromInt (String.length (String.join \",\" (List.map String.fromInt (List.range 1 1))))\n"),
        ("string repeat 50k",
         "main : String\nmain = String.fromInt (String.length (String.repeat 50000 \"ab\"))\n",
         "main : String\nmain = String.fromInt (String.length (String.repeat 1 \"ab\"))\n"),
        ("json encode 100k",
         "import Json.Encode as E\nmain : String\nmain = String.fromInt (String.length (E.encode 0 (E.list E.int (List.range 1 100000))))\n",
         "import Json.Encode as E\nmain : String\nmain = String.fromInt (String.length (E.encode 0 (E.list E.int (List.range 1 1))))\n"),
        ("dict build+get 30k",
         "import Dict\nmain : String\nmain = String.fromInt (Dict.size (Dict.fromList (List.map (\\i -> ( i, i * 2 )) (List.range 1 30000))))\n",
         "import Dict\nmain : String\nmain = String.fromInt (Dict.size (Dict.fromList (List.map (\\i -> ( i, i * 2 )) (List.range 1 1))))\n"),
        // KNOWN O(n²): incremental Dict.insert / Array.push each copy the whole
        // vector (see memory) — small n here just to track the ratio, not hang.
        ("dict incremental 5k [O(n²)]",
         "import Dict\nmain : String\nmain = String.fromInt (Dict.size (List.foldl (\\i d -> Dict.insert i i d) Dict.empty (List.range 1 5000)))\n",
         "import Dict\nmain : String\nmain = String.fromInt (Dict.size (List.foldl (\\i d -> Dict.insert i i d) Dict.empty (List.range 1 1)))\n"),
        ("array push 200k",
         "import Array\nmain : String\nmain = String.fromInt (Array.length (List.foldl Array.push Array.empty (List.range 1 200000)))\n",
         "import Array\nmain : String\nmain = String.fromInt (Array.length (List.foldl Array.push Array.empty (List.range 1 1)))\n"),
        ("string split 100k",
         "main : String\nmain = String.fromInt (List.length (String.split \",\" (String.repeat 100000 \"a,\")))\n",
         "main : String\nmain = String.fromInt (List.length (String.split \",\" (String.repeat 1 \"a,\")))\n"),
        ("record update 500k",
         "type alias R = { a : Int, b : Int }\nstep : Int -> R -> R\nstep i r = { r | a = r.a + i, b = r.b - i }\nmain : String\nmain = String.fromInt ((\\r -> r.a) (List.foldl step { a = 0, b = 0 } (List.range 1 500000)))\n",
         "type alias R = { a : Int, b : Int }\nstep : Int -> R -> R\nstep i r = { r | a = r.a + i, b = r.b - i }\nmain : String\nmain = String.fromInt ((\\r -> r.a) (List.foldl step { a = 0, b = 0 } (List.range 1 1)))\n"),
    ];
    let dir = common::test_dir("alm-wasmgc", "bench");
    for (name, heavy, base) in workloads {
        let mut jc = [0f64; 2];
        let mut wc = [0f64; 2];
        for (ci, body) in [heavy, base].iter().enumerate() {
            let entry = dir.join("Test.elm");
            std::fs::write(&entry, format!("{prelude}{body}")).unwrap();
            let checked = project::check_project(&entry).unwrap_or_else(|_| panic!("check failed: {name}"));
            let bundle = dir.join("b.js");
            std::fs::write(&bundle, generate::generate_project(&checked.modules)).unwrap();
            let wasm = dir.join("a.wasm");
            project::compile_project_wasmgc(&entry, &wasm).unwrap_or_else(|_| panic!("wasm build failed: {name}"));
            let runner = dir.join("r.cjs");
            std::fs::write(&runner, STR_RUNNER).unwrap();
            let best = |mk: &dyn Fn() -> Command| {
                let mut b = f64::MAX;
                for _ in 0..5 {
                    let t = Instant::now();
                    let _ = run(&mut mk());
                    b = b.min(t.elapsed().as_secs_f64() * 1000.0);
                }
                b
            };
            jc[ci] = best(&|| {
                let mut c = Command::new("node");
                c.arg("-e").arg(format!("require({:?}).Test.main", bundle.display()));
                c
            });
            wc[ci] = best(&|| {
                let mut c = Command::new("node");
                c.arg(&runner).arg(&wasm);
                c
            });
        }
        let (js, wg) = (jc[0] - jc[1], wc[0] - wc[1]);
        eprintln!("BENCH {name:24} JS {js:7.1}ms  WasmGC {wg:7.1}ms  ratio {:.2}x", wg / js);
    }
}
