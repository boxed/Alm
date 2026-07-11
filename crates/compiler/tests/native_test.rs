//! End-to-end tests for the native (LLVM) backend: compile a fixture to a
//! binary with clang and compare its stdout with what the JS backend
//! would print.


mod common;

use std::path::PathBuf;
use std::process::Command;

use alm_compiler::{generate, ir, project};

/// Compile `source` (a `module Test ...`) to a native binary and return
/// its stdout.
fn run_native(test_name: &str, source: &str) -> String {
    let dir = common::test_dir("alm-native-tests", test_name);
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
    let program = ir::lower::lower_project(&checked.modules);
    let binary: PathBuf = dir.join("test");
    generate::native::build(&program, &binary, generate::native::Target::Native)
        .unwrap_or_else(|e| panic!("native build failed: {}", e));

    let output = Command::new(&binary).output().expect("run binary");
    assert!(
        output.status.success(),
        "binary failed with {:?}:\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

#[test]
fn string_literal() {
    let out = run_native(
        "string_literal",
        "module Test exposing (..)\n\
         \n\
         main = \"Hello, native!\"\n",
    );
    assert_eq!(out, "Hello, native!");
}

#[test]
fn arithmetic_and_from_int() {
    let out = run_native(
        "arithmetic",
        "module Test exposing (..)\n\
         \n\
         main = String.fromInt (6 * 7 + 10 // 3 - 2 ^ 3)\n",
    );
    assert_eq!(out, "37");
}

#[test]
fn functions_currying_and_captures() {
    let out = run_native(
        "currying",
        "module Test exposing (..)\n\
         \n\
         add x y = x + y\n\
         \n\
         apply f v = f v\n\
         \n\
         main =\n\
         \x20   let\n\
         \x20       inc = add 1\n\
         \x20       n = 40\n\
         \x20       plusN = \\x -> x + n\n\
         \x20   in\n\
         \x20   String.fromInt (apply inc 1 + plusN 0)\n",
    );
    assert_eq!(out, "42");
}

#[test]
fn custom_types_case_and_records() {
    let out = run_native(
        "custom_types",
        "module Test exposing (..)\n\
         \n\
         type Shape\n\
         \x20   = Circle Int\n\
         \x20   | Rect Int Int\n\
         \n\
         area shape =\n\
         \x20   case shape of\n\
         \x20       Circle r -> 3 * r * r\n\
         \x20       Rect w h -> w * h\n\
         \n\
         main =\n\
         \x20   let\n\
         \x20       box = { width = 6, height = 7 }\n\
         \x20       shapes = [ Circle 2, Rect box.width box.height ]\n\
         \x20       total = sum shapes\n\
         \x20   in\n\
         \x20   String.fromInt total\n\
         \n\
         sum shapes =\n\
         \x20   case shapes of\n\
         \x20       [] -> 0\n\
         \x20       shape :: rest -> area shape + sum rest\n",
    );
    assert_eq!(out, "54");
}

#[test]
fn tail_recursion_runs_as_a_loop() {
    // A million iterations would overflow the stack without the loop
    // transform.
    let out = run_native(
        "tail_recursion",
        "module Test exposing (..)\n\
         \n\
         sumTo n acc =\n\
         \x20   if n <= 0 then\n\
         \x20       acc\n\
         \x20   else\n\
         \x20       sumTo (n - 1) (acc + n)\n\
         \n\
         main = String.fromInt (sumTo 1000000 0)\n",
    );
    assert_eq!(out, "500000500000");
}

#[test]
fn strings_ifs_and_comparison() {
    let out = run_native(
        "strings",
        "module Test exposing (..)\n\
         \n\
         describe n =\n\
         \x20   if n < 0 then\n\
         \x20       \"negative\"\n\
         \x20   else if n == 0 then\n\
         \x20       \"zero\"\n\
         \x20   else\n\
         \x20       \"positive\"\n\
         \n\
         main = describe -5 ++ \" \" ++ describe 0 ++ \" \" ++ describe 5\n",
    );
    assert_eq!(out, "negative zero positive");
}

#[test]
fn local_recursion_and_record_update() {
    let out = run_native(
        "letrec",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   let\n\
         \x20       counter = { count = 0, label = \"n\" }\n\
         \x20       bumped = { counter | count = 3 }\n\
         \x20       len xs =\n\
         \x20           case xs of\n\
         \x20               [] -> 0\n\
         \x20               _ :: rest -> 1 + len rest\n\
         \x20   in\n\
         \x20   bumped.label ++ String.fromInt (bumped.count + len [ 1, 2, 3 ])\n",
    );
    assert_eq!(out, "n6");
}
