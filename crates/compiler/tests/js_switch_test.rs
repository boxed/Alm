//! The JS backend compiles a `case` that dispatches on one shared discriminant
//! (a constructor tag or an int/string/char literal, plus an optional trailing
//! catch-all) to a `switch`/jump table instead of a linear `if`/`else if` chain.
//! Anything else falls back to the chain. These tests pin both the codegen shape
//! and that every shape still evaluates correctly.

mod common;

use std::process::Command;

use alm_compiler::{generate, project};

/// Compile `Main.elm` to JS and return `(source, stdout-of-running-main)`.
fn compile_and_run(test_name: &str, source: &str) -> (String, String) {
    let dir = common::test_dir("alm-js-switch", test_name);
    let entry = dir.join("Main.elm");
    std::fs::write(&entry, source).expect("write Main.elm");
    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let js = generate::generate_project(&checked.modules);
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, &js).expect("write bundle");
    let out = Command::new("node")
        .arg("-e")
        .arg(format!("require({:?})['Main']['main'].init({{}})", bundle.display()))
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .output()
        .expect("run node");
    assert!(
        out.status.success(),
        "node failed:\nstderr: {}\n\nJS:\n{}",
        String::from_utf8_lossy(&out.stderr),
        js
    );
    (js, String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn worker(body: &str, observe: &str) -> String {
    format!(
        r#"module Main exposing (main)

{body}

main : Program () () ()
main =
    Platform.worker
        {{ init = \_ -> ( (), Terminal.writeLine ({observe}) )
        , update = \_ m -> ( m, Cmd.none )
        , subscriptions = \_ -> Sub.none
        }}
"#
    )
}

#[test]
fn flat_constructor_dispatch_becomes_a_switch() {
    let src = worker(
        r#"type Msg = Inc | Dec | Set Int | Reset

step : Msg -> Int -> Int
step msg n =
    case msg of
        Inc -> n + 1
        Dec -> n - 1
        Set k -> k
        Reset -> 0"#,
        r#"String.fromInt (step (Set 5) 0 |> step Inc |> step Dec |> step Reset |> step Inc)"#,
    );
    let (js, out) = compile_and_run("ctor", &src);
    assert!(js.contains("case 'Inc':") && js.contains("case 'Set':"), "expected a tag switch:\n{js}");
    assert_eq!(out, "1");
}

#[test]
fn int_literals_with_catch_all_become_a_switch() {
    let src = worker(
        r#"grade : Int -> String
grade n =
    case n of
        0 -> "zero"
        1 -> "one"
        2 -> "two"
        _ -> "many""#,
        r#"grade 0 ++ grade 2 ++ grade 99"#,
    );
    let (js, out) = compile_and_run("int", &src);
    assert!(js.contains("case 0:") && js.contains("case 2:"), "expected an int switch:\n{js}");
    assert!(js.contains("default:"), "expected a default arm:\n{js}");
    assert_eq!(out, "zerotwomany");
}

#[test]
fn string_literals_become_a_switch() {
    let src = worker(
        r#"kind : String -> Int
kind s =
    case s of
        "a" -> 1
        "bb" -> 2
        "ccc" -> 3
        _ -> 0"#,
        r#"String.fromInt (kind "a" + kind "bb" + kind "ccc" + kind "z")"#,
    );
    let (js, out) = compile_and_run("str", &src);
    assert!(js.contains("case 'a':") && js.contains("case 'bb':"), "expected a string switch:\n{js}");
    assert_eq!(out, "6");
}

#[test]
fn repeated_tag_with_literal_arg_falls_back_but_is_correct() {
    // `Just 0` and `Just n` share the `Just` tag with a refutable arg, so this
    // cannot be a clean switch — it must fall back to the if-chain.
    let src = worker(
        r#"describe : Maybe Int -> String
describe m =
    case m of
        Just 0 -> "just zero"
        Just n -> "just " ++ String.fromInt n
        Nothing -> "nothing""#,
        r#"describe (Just 0) ++ "|" ++ describe (Just 7) ++ "|" ++ describe Nothing"#,
    );
    let (js, out) = compile_and_run("repeated", &src);
    assert!(!js.contains("case 'Just':"), "should not switch on a repeated tag:\n{js}");
    assert_eq!(out, "just zero|just 7|nothing");
}

#[test]
fn nested_refutable_arg_falls_back_but_is_correct() {
    let src = worker(
        r#"type Pair = Pair (Maybe Int) Int

sum : Pair -> Int
sum p =
    case p of
        Pair (Just x) y -> x + y
        Pair Nothing y -> y"#,
        r#"String.fromInt (sum (Pair (Just 3) 4) + sum (Pair Nothing 5))"#,
    );
    let (_js, out) = compile_and_run("nested", &src);
    assert_eq!(out, "12");
}

#[test]
fn constructor_dispatch_with_bound_args_binds_correctly_in_switch() {
    // Each arm binds a constructor argument; the switch must bind them per case.
    let src = worker(
        r#"type Shape = Circle Int | Rect Int Int | Dot

area : Shape -> Int
area s =
    case s of
        Circle r -> 3 * r * r
        Rect w h -> w * h
        Dot -> 0"#,
        r#"String.fromInt (area (Circle 2) + area (Rect 3 4) + area Dot)"#,
    );
    let (js, out) = compile_and_run("bound", &src);
    assert!(js.contains("case 'Circle':") && js.contains("case 'Rect':"), "expected a tag switch:\n{js}");
    assert_eq!(out, "24");
}
