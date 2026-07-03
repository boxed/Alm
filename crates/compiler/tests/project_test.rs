//! Multi-module project compilation tests: write a project to a temp
//! directory, compile it with `compile_project`, and run it with node.

use std::path::Path;
use std::process::Command;

fn project(files: &[(&str, &str)]) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!(
        "alm-project-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"] }"#,
    )
    .unwrap();
    for (name, contents) in files {
        let path = src.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    compile_and_run(&src.join(files[0].0), &files[0].1)
}

fn compile_and_run(entry: &Path, _entry_source: &str) -> Result<String, String> {
    let javascript = alm_compiler::project::compile_project(entry)
        .map_err(|errors| {
            errors
                .iter()
                .map(|e| e.render())
                .collect::<Vec<_>>()
                .join("\n")
        })?;
    let js_path = entry.with_extension("js");
    std::fs::write(&js_path, &javascript).unwrap();
    let output = Command::new("node")
        .arg("-e")
        .arg(format!(
            "console.log(require({:?})['Main']['main']);",
            js_path.to_str().unwrap()
        ))
        .output()
        .expect("failed to run node");
    if !output.status.success() {
        return Err(format!(
            "node failed:\n{}\n\nJS:\n{}",
            String::from_utf8_lossy(&output.stderr),
            javascript
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}

#[test]
fn two_modules() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Utils\n\nmain : String\nmain = Utils.greet \"world\"\n",
        ),
        (
            "Utils.elm",
            "module Utils exposing (greet)\n\ngreet : String -> String\ngreet name = \"Hello, \" ++ name ++ \"!\"\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "Hello, world!");
}

#[test]
fn nested_module_names() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Data.Math as Math exposing (double)\n\nmain = String.fromInt (Math.triple 2 + double 3)\n",
        ),
        (
            "Data/Math.elm",
            "module Data.Math exposing (double, triple)\n\ndouble : Int -> Int\ndouble n = n * 2\n\ntriple : Int -> Int\ntriple n = n * 3\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "12");
}

#[test]
fn custom_types_across_modules() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Shape exposing (Shape(..), area)\n\nmain =\n    [ Circle 1.0, Square 2.0 ]\n        |> List.map area\n        |> List.sum\n        |> String.fromFloat\n",
        ),
        (
            "Shape.elm",
            "module Shape exposing (Shape(..), area)\n\ntype Shape\n    = Circle Float\n    | Square Float\n\narea : Shape -> Float\narea shape =\n    case shape of\n        Circle r ->\n            3.0 * r * r\n\n        Square w ->\n            w * w\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "7");
}

#[test]
fn type_aliases_across_modules() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Person exposing (Person)\n\nolder : Person -> Person\nolder p = { p | age = p.age + 1 }\n\nmain =\n    older (Person.make \"Ann\" 39)\n        |> Person.describe\n",
        ),
        (
            "Person.elm",
            "module Person exposing (Person, make, describe)\n\ntype alias Person =\n    { name : String, age : Int }\n\nmake : String -> Int -> Person\nmake name age = { name = name, age = age }\n\ndescribe : Person -> String\ndescribe p = p.name ++ \" is \" ++ String.fromInt p.age\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "Ann is 40");
}

#[test]
fn opaque_types_stay_opaque() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Counter exposing (Counter)\n\nmain = Debug.toString (Counter.increment Counter.zero)\n",
        ),
        (
            "Counter.elm",
            "module Counter exposing (Counter, zero, increment)\n\ntype Counter\n    = Counter Int\n\nzero : Counter\nzero = Counter 0\n\nincrement : Counter -> Counter\nincrement (Counter n) = Counter (n + 1)\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "Counter 1");

    // Using the private constructor from outside must fail.
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Counter exposing (Counter)\n\nmain = Debug.toString (Counter.Counter 5)\n",
        ),
        (
            "Counter.elm",
            "module Counter exposing (Counter, zero)\n\ntype Counter\n    = Counter Int\n\nzero : Counter\nzero = Counter 0\n",
        ),
    ]);
    assert!(result.is_err(), "private constructor should not be usable");
}

#[test]
fn diamond_dependencies() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport A\nimport B\n\nmain = String.fromInt (A.value + B.value)\n",
        ),
        (
            "A.elm",
            "module A exposing (value)\n\nimport Shared\n\nvalue = Shared.base * 2\n",
        ),
        (
            "B.elm",
            "module B exposing (value)\n\nimport Shared\n\nvalue = Shared.base * 3\n",
        ),
        ("Shared.elm", "module Shared exposing (base)\n\nbase = 10\n"),
    ]);
    assert_eq!(result.unwrap(), "50");
}

#[test]
fn import_cycles_are_rejected() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport A\n\nmain = A.x\n",
        ),
        ("A.elm", "module A exposing (x)\n\nimport B\n\nx = B.y\n"),
        ("B.elm", "module B exposing (y)\n\nimport A\n\ny = \"loop\"\n"),
    ]);
    let err = result.unwrap_err();
    assert!(err.contains("IMPORT CYCLE"), "got: {}", err);
}

#[test]
fn missing_module_reports_nicely() {
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nimport DoesNotExist\n\nmain = \"x\"\n",
    )]);
    let err = result.unwrap_err();
    assert!(err.contains("MODULE NOT FOUND"), "got: {}", err);
}

#[test]
fn unexposed_values_are_private() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Utils\n\nmain = Utils.secret\n",
        ),
        (
            "Utils.elm",
            "module Utils exposing (public)\n\npublic = 1\n\nsecret = 2\n",
        ),
    ]);
    assert!(result.is_err(), "unexposed value should be private");
}

#[test]
fn cross_module_type_errors_are_caught() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Utils\n\nmain = Utils.greet 42\n",
        ),
        (
            "Utils.elm",
            "module Utils exposing (greet)\n\ngreet : String -> String\ngreet name = name\n",
        ),
    ]);
    let err = result.unwrap_err();
    assert!(err.contains("TYPE MISMATCH"), "got: {}", err);
}

#[test]
fn inferred_exports_work_without_annotations() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Poly\n\nmain =\n    Debug.toString ( Poly.identity 1, Poly.identity \"a\" )\n",
        ),
        (
            "Poly.elm",
            "module Poly exposing (identity)\n\nidentity x = x\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "(1,\"a\")");
}

#[test]
fn custom_operators() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Ops exposing (Wrap(..), unwrap, (|+|))\n\nmain = String.fromInt (unwrap (Wrap 20 |+| Wrap 22))\n",
        ),
        (
            "Ops.elm",
            "module Ops exposing (Wrap(..), unwrap, (|+|))\n\ninfix left 6 (|+|) = plus\n\n\ntype Wrap\n    = Wrap Int\n\n\nplus : Wrap -> Wrap -> Wrap\nplus (Wrap a) (Wrap b) =\n    Wrap (a + b)\n\n\nunwrap : Wrap -> Int\nunwrap (Wrap n) =\n    n\n",
        ),
    ]);
    assert_eq!(result.unwrap(), "42");
}

#[test]
fn value_recursion_through_lambdas_is_legal() {
    // Like recursive Json decoders: the self-reference is delayed.
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nimport Json.Decode as Decode\n\ntype Tree\n    = Node Int (List Tree)\n\ntreeDecoder : Decode.Decoder Tree\ntreeDecoder =\n    Decode.map2 Node\n        (Decode.field \"v\" Decode.int)\n        (Decode.field \"kids\" (Decode.list (Decode.lazy (\\_ -> treeDecoder))))\n\nsumTree : Tree -> Int\nsumTree (Node n kids) =\n    n + List.sum (List.map sumTree kids)\n\nmain =\n    case Decode.decodeString treeDecoder \"{\\\"v\\\":1,\\\"kids\\\":[{\\\"v\\\":2,\\\"kids\\\":[]},{\\\"v\\\":3,\\\"kids\\\":[]}]}\" of\n        Ok tree ->\n            String.fromInt (sumTree tree)\n\n        Err _ ->\n            \"failed\"\n",
    )]);
    assert_eq!(result.unwrap(), "6");
}

#[test]
fn direct_value_cycles_are_still_rejected() {
    let result = project(&[(
        "Main.elm",
        "module Main exposing (main)\n\nx = y + 1\n\ny = x + 1\n\nmain = String.fromInt x\n",
    )]);
    assert!(result.is_err(), "direct value cycle must be an error");
}

#[test]
fn one_alias_may_cover_several_modules() {
    let result = project(&[
        (
            "Main.elm",
            "module Main exposing (main)\n\nimport Money as M\nimport Prices as M\n\nmain = M.currency ++ String.fromInt M.price\n",
        ),
        ("Money.elm", "module Money exposing (currency)\n\ncurrency = \"SEK\"\n"),
        ("Prices.elm", "module Prices exposing (price)\n\nprice = 99\n"),
    ]);
    assert_eq!(result.unwrap(), "SEK99");
}
