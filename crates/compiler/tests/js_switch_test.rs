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
fn repeated_tag_with_literal_arg_nests_via_decision_tree() {
    // `Just 0` and `Just n` share the `Just` tag: the flat jump table declines,
    // but the decision tree handles it by testing the outer tag once and then
    // switching on the inner literal.
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
    // Outer tag switched once (Just / Nothing), inner literal switched inside.
    assert!(js.contains("case 'Just':") && js.contains("case 'Nothing':"), "expected an outer tag switch:\n{js}");
    assert!(js.contains("case 0:"), "expected an inner literal switch:\n{js}");
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
fn nested_constructors_test_the_outer_tag_once() {
    // The decision tree tests the outer `Just2` tag ONCE, then switches on the
    // inner tag — the sequential form would re-test `Just2` for each inner arm.
    let src = worker(
        r#"type Inner = A Int | B | C Int
type Outer = Wrap Inner | None2

describe : Outer -> String
describe o =
    case o of
        Wrap (A n) -> "A" ++ String.fromInt n
        Wrap B -> "B"
        Wrap (C n) -> "C" ++ String.fromInt n
        None2 -> "none""#,
        r#"String.join "," [ describe (Wrap (A 1)), describe (Wrap B), describe (Wrap (C 3)), describe None2 ]"#,
    );
    let (js, out) = compile_and_run("nested_ctor", &src);
    // `describe`'s body tests the outer tag exactly once.
    let describe = js
        .split("var $Main$describe = ")
        .nth(1)
        .and_then(|s| s.split("; var ").next())
        .unwrap_or(&js);
    assert_eq!(
        describe.matches("'Wrap'").count(),
        1,
        "outer tag should be tested once:\n{describe}"
    );
    assert_eq!(out, "A1,B,C3,none");
}

#[test]
fn tuple_and_list_patterns_via_decision_tree() {
    let src = worker(
        r#"combine : ( Maybe Int, List Int ) -> String
combine pair =
    case pair of
        ( Just x, [] ) -> "j-empty-" ++ String.fromInt x
        ( Just x, y :: _ ) -> "j-" ++ String.fromInt x ++ "-" ++ String.fromInt y
        ( Nothing, [] ) -> "n-empty"
        ( Nothing, y :: _ ) -> "n-" ++ String.fromInt y"#,
        r#"String.join "," [ combine (Just 5, []), combine (Just 6, [7]), combine (Nothing, []), combine (Nothing, [9]) ]"#,
    );
    let (_js, out) = compile_and_run("tuple_list", &src);
    assert_eq!(out, "j-empty-5,j-6-7,n-empty,n-9");
}

#[test]
fn duplicating_match_falls_back_but_is_correct() {
    // The first column (A|B) is exhaustive, so the wildcard third row would be
    // duplicated by a decision tree; it falls back to the if-chain and stays
    // correct.
    let src = worker(
        r#"type T = A | B
type S = C | D

pick : ( T, S ) -> Int
pick ts =
    case ts of
        ( A, C ) -> 1
        ( B, C ) -> 2
        ( _, D ) -> 3"#,
        r#"String.join "," (List.map (String.fromInt << pick) [ (A, C), (B, C), (A, D), (B, D) ])"#,
    );
    let (_js, out) = compile_and_run("dup", &src);
    assert_eq!(out, "1,2,3,3");
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
