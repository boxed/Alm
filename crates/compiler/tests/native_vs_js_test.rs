//! Differential tests: the same fixture compiled with the JS backend
//! (run under node) and the native backend (compiled with clang) must
//! print the same thing. This pins the C kernel to runtime.js semantics.

use std::process::Command;

use alm_compiler::{generate, ir, project};

fn run_both(test_name: &str, source: &str) -> (String, String) {
    let dir = std::env::temp_dir()
        .join("alm-native-vs-js")
        .join(format!("{}-{}", test_name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");

    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!(
            "check failed:\n{}",
            errors
                .iter()
                .map(|e| e.render())
                .collect::<Vec<_>>()
                .join("\n")
        )
    });

    let js = generate::generate_project(&checked.modules);
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, js).expect("write bundle");
    let js_out = run_command(
        Command::new("node").arg("-e").arg(format!(
            "console.log(require({:?})['Test']['main'])",
            bundle.display()
        )),
        "node",
    );

    let program = ir::lower::lower_project(&checked.modules);
    let binary = dir.join("test");
    generate::native::build(&program, &binary)
        .unwrap_or_else(|e| panic!("native build failed: {}", e));
    let native_out = run_command(&mut Command::new(&binary), "native binary");

    (js_out, native_out)
}

fn run_command(command: &mut Command, what: &str) -> String {
    let output = command.output().unwrap_or_else(|e| panic!("run {}: {}", what, e));
    assert!(
        output.status.success(),
        "{} failed with {:?}:\nstdout: {}\nstderr: {}",
        what,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

fn assert_same(test_name: &str, source: &str) {
    let (js, native) = run_both(test_name, source);
    assert!(!js.is_empty(), "JS output is empty");
    assert_eq!(native, js, "native and JS backends disagree");
}

#[test]
fn list_pipeline() {
    assert_same(
        "list_pipeline",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   List.range 1 20\n\
         \x20       |> List.filter (\\n -> modBy 2 n == 0)\n\
         \x20       |> List.map (\\n -> n * n)\n\
         \x20       |> List.map String.fromInt\n\
         \x20       |> String.join \",\"\n",
    );
}

#[test]
fn string_functions() {
    assert_same(
        "string_functions",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   String.join \"|\"\n\
         \x20       [ String.toUpper \"hello\"\n\
         \x20       , String.repeat 3 \"ab\"\n\
         \x20       , String.slice 1 4 \"abcdef\"\n\
         \x20       , String.slice -3 -1 \"abcdef\"\n\
         \x20       , String.padLeft 5 '.' \"x\"\n\
         \x20       , String.padRight 5 '-' \"y\"\n\
         \x20       , String.replace \"l\" \"L\" \"hello world\"\n\
         \x20       , String.join \"+\" (String.split \",\" \"a,b,,c\")\n\
         \x20       , String.join \"_\" (String.words \"  lots   of words \")\n\
         \x20       , String.trim \"  mid  \"\n\
         \x20       , String.left 2 \"abcdef\"\n\
         \x20       , String.right 2 \"abcdef\"\n\
         \x20       , String.dropLeft 2 \"abcdef\"\n\
         \x20       , String.dropRight 2 \"abcdef\"\n\
         \x20       , String.reverse \"stressed\"\n\
         \x20       ]\n",
    );
}

#[test]
fn maybe_and_result_via_debug() {
    assert_same(
        "maybe_result",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   Debug.toString\n\
         \x20       ( ( String.toInt \"42\", String.toInt \"007\", String.toInt \"-0\" )\n\
         \x20       , ( String.toFloat \"2.5\", String.toFloat \"nope\" )\n\
         \x20       , ( Maybe.map (\\n -> n + 1) (Just 1)\n\
         \x20         , Maybe.withDefault 0 Nothing\n\
         \x20         , Result.map String.length (Ok \"four\")\n\
         \x20         )\n\
         \x20       )\n",
    );
}

#[test]
fn sorting() {
    assert_same(
        "sorting",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   Debug.toString\n\
         \x20       ( List.sort [ 5, 3, 8, 1, 9, 2 ]\n\
         \x20       , List.sortBy String.length [ \"ccc\", \"a\", \"bb\" ]\n\
         \x20       , List.sortWith (\\a b -> compare b a) [ 5, 3, 8, 1 ]\n\
         \x20       )\n",
    );
}

#[test]
fn debug_to_string_of_nested_values() {
    assert_same(
        "debug_nested",
        "module Test exposing (..)\n\
         \n\
         type Shape\n\
         \x20   = Circle Float\n\
         \x20   | Rect Int Int\n\
         \x20   | Named String Shape\n\
         \n\
         main =\n\
         \x20   Debug.toString\n\
         \x20       { shapes = [ Circle 1.5, Rect 2 3, Named \"box\" (Rect 1 1) ]\n\
         \x20       , pair = ( 'x', \"quo\\\"te\" )\n\
         \x20       , flag = True\n\
         \x20       , unit = ()\n\
         \x20       }\n",
    );
}

#[test]
fn folds_and_higher_order_functions() {
    assert_same(
        "folds",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   String.join \"|\"\n\
         \x20       [ Debug.toString (List.foldl (\\x acc -> x :: acc) [] [ 1, 2, 3 ])\n\
         \x20       , Debug.toString (List.foldr (\\x acc -> x - acc) 0 [ 1, 2, 3 ])\n\
         \x20       , Debug.toString (List.indexedMap Tuple.pair [ \"a\", \"b\" ])\n\
         \x20       , Debug.toString (List.map2 (\\a b -> a * b) [ 1, 2, 3 ] [ 4, 5 ])\n\
         \x20       , Debug.toString (List.concatMap (\\n -> [ n, n ]) [ 1, 2 ])\n\
         \x20       , Debug.toString (List.partition (\\n -> n > 2) [ 1, 2, 3, 4 ])\n\
         \x20       , Debug.toString (List.unzip [ ( 1, \"a\" ), ( 2, \"b\" ) ])\n\
         \x20       , Debug.toString (List.intersperse 0 [ 1, 2, 3 ])\n\
         \x20       ]\n",
    );
}

#[test]
fn chars_and_string_traversal() {
    assert_same(
        "chars",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   String.join \"|\"\n\
         \x20       [ Debug.toString (Char.toCode 'A')\n\
         \x20       , Debug.toString (Char.fromCode 66)\n\
         \x20       , Debug.toString (String.toList \"abc\")\n\
         \x20       , String.fromList [ 'x', 'y' ]\n\
         \x20       , String.map Char.toUpper \"mixed Case\"\n\
         \x20       , String.filter Char.isDigit \"a1b2c3\"\n\
         \x20       , Debug.toString (String.uncons \"hi\")\n\
         \x20       , Debug.toString (String.any Char.isUpper \"abC\")\n\
         \x20       , Debug.toString (String.all Char.isLower \"abc\")\n\
         \x20       ]\n",
    );
}

#[test]
fn numeric_edge_cases() {
    assert_same(
        "numerics",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   String.join \"|\"\n\
         \x20       [ Debug.toString ( modBy 4 -1, remainderBy 4 -1, -13 // 4 )\n\
         \x20       , Debug.toString ( round -2.5, floor -2.5, ceiling -2.5 )\n\
         \x20       , Debug.toString ( truncate -2.5, abs -7, clamp 1 10 42 )\n\
         \x20       , Debug.toString ( min 3 7, max 3 7 )\n\
         \x20       , Debug.toString ( sqrt 2, 2 ^ 10, String.fromFloat (10 / 4) )\n\
         \x20       , Debug.toString ( compare 1 2, compare \"b\" \"a\", compare [ 1, 2 ] [ 1, 2 ] )\n\
         \x20       ]\n",
    );
}

#[test]
fn composition_operators() {
    assert_same(
        "composition",
        "module Test exposing (..)\n\
         \n\
         shout = String.toUpper << String.trim\n\
         \n\
         exclaim = String.trim >> (\\s -> s ++ \"!\")\n\
         \n\
         main = shout \"  quiet  \" ++ \" \" ++ exclaim \"  loud  \"\n",
    );
}

#[test]
fn maximum_minimum_sum_product() {
    assert_same(
        "aggregates",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   String.join \"|\"\n\
         \x20       [ Debug.toString (List.maximum [ 3, 9, 2 ])\n\
         \x20       , Debug.toString (List.minimum [ [ 2 ], [ 1, 3 ] ])\n\
         \x20       , Debug.toString (List.sum [ 1, 2, 3 ])\n\
         \x20       , Debug.toString (List.sum [ 1.5, 2.5 ])\n\
         \x20       , Debug.toString (List.product [ 2, 3, 4 ])\n\
         \x20       , Debug.toString (List.member 3 [ 1, 2, 3 ])\n\
         \x20       , Debug.toString (List.take 2 [ 1, 2, 3, 4 ])\n\
         \x20       , Debug.toString (List.drop 2 [ 1, 2, 3, 4 ])\n\
         \x20       ]\n",
    );
}
