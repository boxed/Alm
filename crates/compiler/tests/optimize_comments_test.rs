//! `alm make --optimize` ships a bundle with no comments in it.
//!
//! The risk in stripping comments from JavaScript is never the comments — it is
//! everything that *looks* like one: `'http://…'`, `/[/*]/`, the minified
//! `marked` build the markdown kernel vendors. So the test that matters is not
//! that the comments are gone but that the program still behaves identically
//! with them gone, over as much of the kernel as can be made to run.

mod common;

use common::{compile_single, compile_single_no_dce, run_node, test_dir, write_js};

use alm_compiler::generate::comments::strip;

/// A program that puts a broad slice of the kernel through its paces. Compiled
/// with DCE off, the bundle also *contains* every kernel definition, including
/// the vendored markdown parser — which is a top-level IIFE, so merely loading
/// the bundle evaluates its several hundred regex literals.
const PROGRAM: &str = r#"
module Main exposing (main)

import Dict
import Set
import Json.Decode as D
import Json.Encode as E


main : String
main =
    String.join "|"
        [ String.toUpper "http://example.com/a//b"
        , String.fromInt (List.sum (List.map (\n -> n // 2) (List.range 1 10)))
        , String.fromFloat (7 / 2)
        , Dict.fromList [ ( "a", 1 ), ( "b", 2 ) ] |> Dict.toList |> Debug.toString
        , Set.fromList [ 3, 1, 2 ] |> Set.toList |> Debug.toString
        , String.reverse "/* not a comment */"
        , E.encode 0 (E.object [ ( "x", E.string "// nor this" ) ])
        , D.decodeString (D.field "x" D.string) "{\"x\":\"/**/\"}" |> Debug.toString
        , String.filter Char.isDigit "a1b2c3"
        ]
"#;

#[test]
fn stripping_does_not_change_what_the_kernel_does() {
    let javascript = compile_single_no_dce("Main.elm", PROGRAM);
    let stripped = strip(&javascript);

    assert!(stripped.len() < javascript.len(), "nothing was stripped at all");

    let run = |js: &str, tag: &str| {
        let path = write_js(tag, js);
        run_node(&format!("console.log(require({:?}).Elm.Main.main)", path.display()), js)
    };
    assert_eq!(run(&stripped, "stripped"), run(&javascript, "commented"));
}

#[test]
fn stripping_is_idempotent() {
    // A second pass has nothing left to remove — which is only true if the
    // first pass left no comment behind and invented no new one.
    let javascript = compile_single_no_dce("Main.elm", PROGRAM);
    let once = strip(&javascript);
    assert_eq!(strip(&once), once);
}

#[test]
fn an_optimized_build_has_no_comments() {
    let dir = test_dir("optimize-comments", "no-comments");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"], "elm-version": "0.19.1",
            "dependencies": { "direct": { "elm/core": "1.0.5" }, "indirect": {} },
            "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    let entry = src.join("Main.elm");
    // `--optimize` refuses to build over a surviving `Debug` call, so this one
    // is the program above with those lines taken out.
    std::fs::write(
        &entry,
        r#"
module Main exposing (main)

main : String
main =
    String.toUpper "http://example.com" ++ String.fromInt (7 // 2)
"#,
    )
    .unwrap();

    let build = |optimize| match alm_compiler::project::compile_project_with(&entry, optimize) {
        Ok((javascript, _)) => javascript,
        Err(errors) => panic!(
            "compilation failed:\n{}",
            errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
        ),
    };
    let optimized = build(true);
    let plain = build(false);

    let comment_lines = |js: &str| js.lines().filter(|l| l.trim_start().starts_with("//")).count();
    assert!(comment_lines(&plain) > 50, "expected the kernel's comments in a plain build");
    for (n, line) in optimized.lines().enumerate() {
        let line = line.trim_start();
        assert!(
            !line.starts_with("//") && !line.starts_with("/*"),
            "line {} of the optimized bundle is a comment: {line}",
            n + 1
        );
    }

    let path = write_js("optimized", &optimized);
    assert_eq!(
        run_node(&format!("console.log(require({:?}).Elm.Main.main)", path.display()), &optimized),
        "HTTP://EXAMPLE.COM3"
    );
}

#[test]
fn a_plain_build_keeps_its_comments() {
    // The kernel is commented on purpose; only `--optimize` takes them out.
    assert!(compile_single("Main.elm", PROGRAM).contains("//"));
}

#[test]
fn the_vendored_parsers_license_survives() {
    // Dropping an MIT attribution out of a shipped bundle is not a size win
    // worth having.
    let stripped = strip(alm_compiler::generate::RUNTIME);
    assert!(stripped.contains("Copyright (c) 2011-2014, Christopher Jeffrey. (MIT Licensed)"));
}
