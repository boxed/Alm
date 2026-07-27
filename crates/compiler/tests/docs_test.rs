//! `--docs=<file>`: a package's `docs.json`.
//!
//! The format the package website renders and `elm diff`/`elm bump` compare:
//! an array of modules listing unions, aliases, values and binops with their
//! doc comments and types, every type name qualified by the module that
//! defines it. Only entries named in a module's `@docs` lines appear.
//!
//! The fixture below is compared against `elm make --docs` output for the same
//! source, transcribed here so the test needs no elm binary.

use alm_compiler::docs;
use alm_compiler::project;

mod common;

/// `generate_docs`, panicking with rendered reports rather than `Debug`.
fn docs_of(entry: &std::path::Path) -> String {
    project::generate_docs(entry).unwrap_or_else(|errors| {
        panic!("{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    })
}

const PROBE: &str = r#"module Probe exposing (Color(..), Point, double, origin, toLabel)

{-| A probe module.

@docs double, Color, Point, origin, toLabel

-}


{-| Double a number.
-}
double : Int -> Int
double n =
    n * 2


{-| A color.
-}
type Color
    = Red
    | Green Int


{-| A point.
-}
type alias Point =
    { x : Int, y : Int }


{-| The origin.
-}
origin : Point
origin =
    { x = 0, y = 0 }


{-| Label a color.
-}
toLabel : Color -> String
toLabel c =
    case c of
        Red ->
            "red"

        Green n ->
            String.fromInt n
"#;

/// Exactly what `elm make --docs=docs.json` writes for `PROBE`.
const EXPECTED: &str = concat!(
    r#"[{"name":"Probe","comment":" A probe module.\n\n@docs double, Color, Point, origin, toLabel\n\n","#,
    r#""unions":[{"name":"Color","comment":" A color.\n","args":[],"cases":[["Red",[]],["Green",["Basics.Int"]]]}],"#,
    r#""aliases":[{"name":"Point","comment":" A point.\n","args":[],"type":"{ x : Basics.Int, y : Basics.Int }"}],"#,
    r#""values":[{"name":"double","comment":" Double a number.\n","type":"Basics.Int -> Basics.Int"},"#,
    r#"{"name":"origin","comment":" The origin.\n","type":"Probe.Point"},"#,
    r#"{"name":"toLabel","comment":" Label a color.\n","type":"Probe.Color -> String.String"}],"#,
    r#""binops":[]}]"#
);

fn package(modules: &[(&str, &str)], exposed: &str) -> common::TestDir {
    let dir = common::test_dir("alm-docs", "pkg");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        format!(
            r#"{{ "type": "package", "name": "test/probe", "summary": "s",
                 "license": "BSD-3-Clause", "version": "1.0.0",
                 "exposed-modules": [{exposed}],
                 "elm-version": "0.19.0 <= v < 0.20.0",
                 "dependencies": {{ "elm/core": "1.0.0 <= v < 2.0.0" }},
                 "test-dependencies": {{}} }}"#
        ),
    )
    .unwrap();
    for (name, text) in modules {
        std::fs::write(src.join(format!("{name}.elm")), text).unwrap();
    }
    dir
}

#[test]
fn docs_match_elm() {
    let dir = package(&[("Probe", PROBE)], "\"Probe\"");
    let json = docs_of(&dir.join("src/Probe.elm"));
    assert_eq!(json, EXPECTED);
}

/// Every exposed module is documented, not only the entry file's imports.
#[test]
fn all_exposed_modules_are_documented() {
    let one = "module One exposing (a)\n\n{-| One.\n\n@docs a\n\n-}\n\n\n{-| a\n-}\na : Int\na =\n    1\n";
    let two = "module Two exposing (b)\n\n{-| Two.\n\n@docs b\n\n-}\n\n\n{-| b\n-}\nb : Int\nb =\n    2\n";
    let dir = package(&[("One", one), ("Two", two)], "\"One\", \"Two\"");
    // Entry is One, which does not import Two.
    let json = docs_of(&dir.join("src/One.elm"));
    assert!(json.contains("\"name\":\"One\""), "One missing from {json}");
    assert!(json.contains("\"name\":\"Two\""), "Two missing from {json}");
}

/// An `@docs` list may wrap onto the next line — elm/bytes does this, and
/// missing it silently drops entries from the published API.
#[test]
fn docs_lists_may_wrap_across_lines() {
    let comment = " Head.\n\n@docs alpha, beta,\n  gamma, delta\n\n";
    assert_eq!(docs::docs_order(comment), vec!["alpha", "beta", "gamma", "delta"]);
}

/// `@docs Thing(..)` publishes the type, and the constructors are recorded
/// separately, so the `(..)` is not part of the name.
#[test]
fn docs_entries_drop_the_variant_marker() {
    assert_eq!(docs::docs_order(" x\n\n@docs Color(..), value\n"), vec!["Color", "value"]);
}

/// A declaration's comment is the one directly above it. Prose inside another
/// comment must not be mistaken for a declaration — elm/random's docs for
/// `generate` were lost to a line reading "generate random boolean values:".
#[test]
fn prose_inside_a_comment_is_not_a_declaration() {
    let source = "module M exposing (generate)\n\n{-| M.\n\n@docs generate\n\n-}\n\n\n\
                  {-| Transform values. For example, we can\ngenerate random booleans:\n-}\n\
                  generate : Int -> Int\ngenerate n =\n    n\n";
    let dir = package(&[("M", source)], "\"M\"");
    let json = docs_of(&dir.join("src/M.elm"));
    assert!(
        json.contains("Transform values."),
        "the real doc comment should be attached:\n{json}"
    );
}

/// Types are qualified by the module that defines them, and parenthesised only
/// where precedence needs it.
#[test]
fn types_render_the_way_docs_json_writes_them() {
    let source = "module M exposing (f, g, h)\n\n{-| M.\n\n@docs f, g, h\n\n-}\n\n\n\
                  {-| f -}\nf : (Int -> Int) -> List Int -> Maybe Int\nf _ _ =\n    Nothing\n\n\n\
                  {-| g -}\ng : List (Maybe Int) -> ( Int, String )\ng _ =\n    ( 1, \"\" )\n\n\n\
                  {-| h -}\nh : { x : Int } -> ()\nh _ =\n    ()\n";
    let dir = package(&[("M", source)], "\"M\"");
    let json = docs_of(&dir.join("src/M.elm"));
    assert!(
        json.contains(
            r#""type":"(Basics.Int -> Basics.Int) -> List.List Basics.Int -> Maybe.Maybe Basics.Int""#
        ),
        "function argument should be parenthesised:\n{json}"
    );
    assert!(
        json.contains(r#""type":"List.List (Maybe.Maybe Basics.Int) -> ( Basics.Int, String.String )""#),
        "nested application should be parenthesised:\n{json}"
    );
    assert!(json.contains(r#""type":"{ x : Basics.Int } -> ()""#), "record/unit:\n{json}");
}
