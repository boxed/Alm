//! Snapshot tests for compiler diagnostics: each bad program must fail
//! with a message containing a distinctive phrase. This pins the wording
//! of user-facing errors and exercises the error branches that the
//! happy-path suites never reach.

mod common;

/// Compile a single module and return the concatenated error messages.
fn errors_of(body: &str) -> String {
    let source = format!("module Test exposing (..)\n\n{}", body);
    match alm_compiler::compile(&source) {
        Ok(_) => panic!("expected a compile error for:\n{}", body),
        Err(reports) => reports
            .iter()
            .map(|r| format!("{}: {}", r.title, r.message))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Compile a module with a full header (for header-level errors).
fn errors_of_module(source: &str) -> String {
    match alm_compiler::compile(source) {
        Ok(_) => panic!("expected a compile error for:\n{}", source),
        Err(reports) => reports
            .iter()
            .map(|r| format!("{}: {}", r.title, r.message))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn expect(body: &str, phrase: &str) {
    let errors = errors_of(body);
    assert!(
        errors.contains(phrase),
        "expected error containing {:?}, got:\n{}",
        phrase,
        errors
    );
}

fn expect_module(source: &str, phrase: &str) {
    let errors = errors_of_module(source);
    assert!(
        errors.contains(phrase),
        "expected error containing {:?}, got:\n{}",
        phrase,
        errors
    );
}

// NAMING — variables, constructors, modules

#[test]
fn unknown_names() {
    expect("x = missing\n", "I cannot find a `missing` variable");
    expect("x = Missing\n", "I cannot find a `Missing` constructor");
    expect(
        "x = String.missing \"s\"\n",
        "The `String` module does not expose a value named `missing`",
    );
    expect(
        "x = String.Missing\n",
        "The `String` module does not have a `Missing` constructor",
    );
    expect(
        "x = Nope.thing\n",
        "I cannot find a module named `Nope`",
    );
    expect(
        "x : Nope.Thing\nx = 1\n",
        "I cannot find a module named `Nope`",
    );
    expect("x : Missing\nx = 1\n", "I cannot find a type named `Missing`");
    expect(
        "x : String.Missing\nx = 1\n",
        "The `String` module does not have a type named `Missing`",
    );
}

#[test]
fn unknown_operator() {
    expect("x = 1 <=> 2\n", "I do not recognize the `<=>` operator");
    expect("f = (<=>)\n", "I do not recognize the `<=>` operator");
}

// DUPLICATES

#[test]
fn duplicate_definitions() {
    expect(
        "x = 1\n\nx = 2\n",
        "This module has multiple definitions named `x`",
    );
    expect(
        "type A\n    = A1\n\ntype alias A =\n    Int\n\nx = 1\n",
        "This module has multiple types named `A`",
    );
    expect(
        "type A\n    = Dup\n    | Dup\n\nx = 1\n",
        "defines the constructor `Dup` more than once",
    );
    expect(
        "x = { a = 1, a = 2 }\n",
        "This record has the field `a` more than once",
    );
    expect(
        "f ( a, a ) = a\n\nx = f ( 1, 2 )\n",
        "The name `a` is bound more than once in this pattern",
    );
    expect(
        "x =\n    let\n        y =\n            1\n\n        y =\n            2\n    in\n    y\n",
        "This `let` defines `y` more than once",
    );
}

// TYPES AND ALIASES

#[test]
fn alias_problems() {
    expect(
        "type alias Loop =\n    Loop\n\nx : Loop\nx = 1\n",
        "This type alias is recursive",
    );
    expect(
        "type alias Pair a b =\n    ( a, b )\n\nx : Pair Int\nx = ( 1, 2 )\n",
        "The `Pair` type alias needs 2 arguments, but I see 1",
    );
}

#[test]
fn tuple_size_limits() {
    expect(
        "x = ( 1, 2, 3, 4 )\n",
        "Tuples can only hold two or three values",
    );
    expect(
        "x : ( Int, Int, Int, Int )\nx = ( 1, 2, 3 )\n",
        "Tuples can only hold two or three values",
    );
    expect(
        "f ( a, b, c, d ) = a\n\nx = f 1\n",
        "Tuples can only hold two or three values",
    );
}

#[test]
fn ctor_arity() {
    expect(
        "type T\n    = T Int\n\nf v =\n    case v of\n        T ->\n            0\n\nx = f (T 1)\n",
        "The `T` constructor needs 1 argument, but I see 0",
    );
}

// CYCLES

#[test]
fn value_cycles() {
    expect(
        "x = x + 1\n",
        "The value `x` is defined in terms of itself",
    );
    expect(
        "a = b + 1\n\nb = a + 1\n",
        "part of a definition cycle",
    );
    expect(
        "x =\n    let\n        y =\n            y + 1\n    in\n    y\n",
        "The value `y` is defined in terms of itself",
    );
    expect(
        "x =\n    let\n        ( a, b ) =\n            ( b, a )\n    in\n    a\n",
        "destructuring refers to a name it binds",
    );
}

// OPERATORS

#[test]
fn non_associative_chains() {
    expect(
        "x = 1 == 2 == 3\n",
        "cannot chain the non-associative operators",
    );
    expect(
        "x = 1 < 2 > 3\n",
        "cannot chain the non-associative operators",
    );
}

// EXPOSING VALIDATION

#[test]
fn exposing_problems() {
    expect_module(
        "module Test exposing (missing)\n\nx = 1\n",
        "You are trying to expose `missing`, but it is not defined",
    );
    expect_module(
        "module Test exposing (Missing)\n\nx = 1\n",
        "You are trying to expose a type named `Missing`, but it is not defined",
    );
    expect_module(
        "module Test exposing (Point(..))\n\ntype alias Point =\n    { x : Int }\n",
        "`Point` is a type alias; expose it as `Point` without `(..)`",
    );
    expect_module(
        "module Test exposing ((|+|))\n\nx = 1\n",
        "You are trying to expose the `|+|` operator, but it is not defined",
    );
}

#[test]
fn import_exposing_problems() {
    expect_module(
        "module Test exposing (..)\n\nimport String exposing (missing)\n\nx = 1\n",
        "The `String` module does not expose a value named `missing`",
    );
}

// PORTS

#[test]
fn port_module_required() {
    expect_module(
        "module Test exposing (..)\n\nport send : String -> Cmd msg\n\nx = 1\n",
        "Switch this to say port module instead",
    );
}

#[test]
fn bad_infix_declaration() {
    expect_module(
        "module Test exposing (..)\n\ninfix left 6 (|+|) = missing\n\nx = 1\n",
        "points at `missing`, but that function is not defined",
    );
}

// PARSE ERRORS — the pattern grammar's error branches

#[test]
fn pattern_parse_errors() {
    expect(
        "f _x = 1\n\ny = f 2\n",
        "Wildcard patterns must be a lone underscore",
    );
    expect(
        "f v =\n    case v of\n        1.5 ->\n            0\n\n        _ ->\n            1\n\nx = f 1.5\n",
        "I cannot pattern match with floating point numbers",
    );
    expect(
        "f v =\n    case v of\n        [ a b ->\n            0\n\nx = 1\n",
        "I was expecting a closing square bracket to end this list pattern",
    );
    expect(
        "f { a b } = a\n\nx = 1\n",
        "I was expecting a `,` or `}` in this record pattern",
    );
    expect(
        "f ( a b = a\n\nx = 1\n",
        "I was expecting a `,` or `)` in this pattern",
    );
    expect(
        "f let = 1\n\nx = 2\n",
        "is reserved in Elm",
    );
}

#[test]
fn misc_parse_errors() {
    expect("x = 'ab'\n", "closing single quote");
    expect("x = \"unclosed\n", "closing double quote");
    expect("x = {- never closed\n", "I cannot find the end of this multi-line comment");
    expect("x = 99999999999999999999999\n", "out of the range");
    expect("x = \"\\q\"\n", "not a valid escape");
    expect("x = 0x\n", "I thought I was reading a hexidecimal number");
}

// TOP-LEVEL SHAPE

#[test]
fn top_level_destructuring_rejected() {
    expect_module(
        "module Test exposing (..)\n\n( a, b ) = ( 1, 2 )\n",
        "",
    );
}

#[test]
fn annotation_type_must_exist_in_let() {
    expect(
        "x =\n    let\n        y : Missing\n        y =\n            1\n    in\n    y\n",
        "I cannot find a type named `Missing`",
    );
}

// COVERAGE: more parser error branches

#[test]
fn expression_parse_errors() {
    expect("x = (+ \n", "I was expecting a closing parenthesis here");
    expect(
        "x =\n    case 1 of\n        1 ->\n            2\n        oops~\n",
        "",
    );
    expect("x = [ 1, 2\ny = 3\n", "I cannot find the end of this list");
    expect("x = ( 1, 2\ny = 3\n", "");
    expect("x = { a = 1 b = 2 }\n", "I was expecting to see a closing curly brace next");
    expect("x = { a 1 }\n", "I was expecting to see an equals sign next");
    expect("x = .\n", "I am trying to parse a record accessor here");
}

#[test]
fn string_and_escape_errors() {
    expect("x = \"\"\"never closed\n", "closing `\"\"\"`");
    // Lone surrogate escapes are valid Elm (UTF-16 strings); see the e2e test
    // `lone_surrogate_escapes_round_trip_like_elm`.
    expect("x = \"\\u{}\"\n", "hex digits");
    expect("x = \"\\u{41\"\n", "closing `}`");
    expect("x = ''\n", "Please switch to double quotes instead");
}

#[test]
fn type_parse_errors() {
    expect("x : { a : Int\nx = 1\n", "record type");
    expect("x : ( Int, )\nx = 1\n", "Expecting a type");
    expect("x : { a | }\nx = 1\n", "");
    expect("f : Int ->\nf = 1\n", "I was expecting a type after this `->`");
}

#[test]
fn module_header_errors() {
    expect_module("module exposing (..)\n\nx = 1\n", "");
    expect_module(
        "module Test exposing (x, )\n\nx = 1\n",
        "",
    );
    expect_module(
        "module Test exposing (..)\n\nimport \n\nx = 1\n",
        "",
    );
    expect_module(
        "module Test exposing (..)\n\ninfix sideways 5 (|?|) = f\n\nf = 1\n",
        "I was expecting `left`, `right`, or `non`",
    );
    expect_module(
        "module Test exposing (..)\n\ninfix left 99 (|?|) = f\n\nf = 1\n",
        "Precedence must be an integer from 0 to 9",
    );
}

#[test]
fn pattern_parse_error_branches() {
    // `as` with a non-name after it
    expect(
        "f v =\n    case v of\n        ( 1, _ ) as 5 ->\n            0\n\nx = 1\n",
        "",
    );
    // cons pattern with garbage after ::
    expect(
        "f v =\n    case v of\n        x :: ->\n            0\n\nx = 1\n",
        "",
    );
}

// COVERAGE: remaining canonicalize/typecheck error branches

#[test]
fn type_error_through_compile() {
    expect("x : String\nx = 1\n", "I needed");
}

#[test]
fn single_file_unknown_import() {
    expect_module(
        "module Test exposing (..)\n\nimport Absolutely.Missing\n\nx = 1\n",
        "I cannot find a module named `Absolutely.Missing`",
    );
}

#[test]
fn builtin_open_and_ctor_exposing() {
    // These must compile: builtin `exposing (..)` and `Union(..)` imports.
    let ok = alm_compiler::compile(
        "module Test exposing (..)\n\nimport Maybe exposing (..)\nimport Result exposing (Result(..))\n\nf : Maybe Int -> Result String Int\nf m =\n    case m of\n        Just n ->\n            Ok n\n\n        Nothing ->\n            Err \"none\"\n",
    );
    assert!(ok.is_ok(), "builtin exposing forms should compile");
}

#[test]
fn duplicate_port_names() {
    expect_module(
        "port module Test exposing (..)\n\nport send : String -> Cmd msg\n\nsend : String -> Cmd msg\nsend s = Cmd.none\n\nx = 1\n",
        "multiple definitions named `send`",
    );
}

#[test]
fn union_and_port_type_errors() {
    expect(
        "type T\n    = T Missing\n\nx = 1\n",
        "I cannot find a type named `Missing`",
    );
    expect_module(
        "port module Test exposing (..)\n\nport bad : Missing -> Cmd msg\n\nx = 1\n",
        "I cannot find a type named `Missing`",
    );
}

#[test]
fn bad_alias_in_open_exposing_is_skipped() {
    // The alias body fails to canonicalize; open exposing skips it rather
    // than crashing, and the module still compiles.
    let result = alm_compiler::compile(
        "module Test exposing (..)\n\ntype alias Broken =\n    AbsolutelyMissing\n\nx = 1\n",
    );
    assert!(result.is_ok());
}

#[test]
fn record_pattern_duplicate_fields() {
    expect(
        "f { a, a } = a\n\nx = f { a = 1 }\n",
        "bound more than once",
    );
}

#[test]
fn alias_used_as_ctor_must_be_record() {
    expect(
        "type alias I =\n    Int\n\nx = I\n",
        "I cannot find a `I` constructor",
    );
}

#[test]
fn builtin_record_alias_as_constructor() {
    // Http.Metadata is a builtin record alias: usable as a constructor.
    let result = alm_compiler::compile(
        "module Test exposing (..)\n\nmeta = Http.Metadata \"http://x\" 200 \"OK\" Dict.empty\n\nmain = meta.statusText\n",
    );
    assert!(result.is_ok(), "{:?}", result.err().map(|e| e[0].message.clone()));
}

#[test]
fn tuple_in_alias_body() {
    expect(
        "type alias Q =\n    ( Int, Int, Int, Int )\n\nx : Q\nx = Debug.todo \"\"\n",
        "Tuples can only hold two or three values",
    );
}

#[test]
fn builtin_alias_arity_error() {
    expect(
        "x : Svg.Svg\nx = Debug.todo \"\"\n",
        "The `Svg` type alias needs 1 argument",
    );
}

// COVERAGE: let-block cycle diagnostics and destructure patterns

#[test]
fn let_cycle_diagnostics() {
    expect(
        "x =\n    let\n        a =\n            b + 1\n\n        b =\n            a + 1\n    in\n    a\n",
        "part of a definition cycle",
    );
    expect(
        "x =\n    let\n        ( a, b ) =\n            ( c, c )\n\n        c =\n            a\n    in\n    a\n",
        "destructuring is part of a definition cycle",
    );
    // A destructure on a full-dependency cycle whose references are all
    // delayed still gets rejected (destructures cannot be recursive).
    expect(
        "x =\n    let\n        ( f, g ) =\n            ( \\v -> g v, \\w -> f w )\n    in\n    f 1\n",
        "destructuring",
    );
}

#[test]
fn destructure_pattern_shapes_flow_through_sorting() {
    // Record, alias, and single-ctor patterns in let destructures bind
    // names that participate in dependency sorting.
    let ok = alm_compiler::compile(
        "module Test exposing (..)\n\ntype Wrap\n    = Wrap Int\n\nx =\n    let\n        total =\n            named + aliased + unwrapped\n\n        { named } =\n            { named = 1 }\n\n        (( a, b ) as pair) =\n            ( 2, 3 )\n\n        aliased =\n            a + b + Tuple.first pair\n\n        (Wrap unwrapped) =\n            Wrap 4\n    in\n    total\n",
    );
    assert!(ok.is_ok(), "{:?}", ok.err().map(|e| e[0].message.clone()));
}

#[test]
fn refutable_lambda_argument() {
    expect(
        "f = \\(Just x) -> x\n\ny = f (Just 1)\n",
        "Argument patterns must match everything",
    );
}

#[test]
fn unclosed_char_and_trailing_escape() {
    expect("x = 'a\n", "");
    expect("x = \"oops\\", "");
}
