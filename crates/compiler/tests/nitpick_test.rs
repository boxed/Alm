//! Exhaustiveness and redundancy checking tests.

fn check(body: &str) -> Result<(), String> {
    let source = format!("module Test exposing (..)\n\n{}", body);
    match alm_compiler::compile(&source) {
        Ok(_) => Ok(()),
        Err(reports) => Err(reports
            .iter()
            .map(|r| format!("{}: {}", r.title, r.message))
            .collect::<Vec<_>>()
            .join("\n")),
    }
}

fn ok(body: &str) {
    if let Err(err) = check(body) {
        panic!("expected success, got:\n{}", err);
    }
}

fn missing(body: &str, expected_in_message: &str) {
    match check(body) {
        Ok(()) => panic!("expected MISSING PATTERNS error for:\n{}", body),
        Err(err) => {
            assert!(err.contains("MISSING PATTERNS"), "wrong error: {}", err);
            assert!(
                err.contains(expected_in_message),
                "error should mention `{}`, got:\n{}",
                expected_in_message,
                err
            );
        }
    }
}

#[test]
fn complete_case_is_fine() {
    ok("type Color\n    = Red\n    | Green\n    | Blue\n\nf c =\n    case c of\n        Red ->\n            1\n\n        Green ->\n            2\n\n        Blue ->\n            3\n");
    ok("f m =\n    case m of\n        Just x ->\n            x\n\n        Nothing ->\n            0\n");
    ok("f n =\n    case n of\n        0 ->\n            1\n\n        _ ->\n            n\n");
}

#[test]
fn missing_constructor_is_reported() {
    missing(
        "type Color\n    = Red\n    | Green\n    | Blue\n\nf c =\n    case c of\n        Red ->\n            1\n\n        Green ->\n            2\n",
        "Blue",
    );
    missing(
        "f m =\n    case m of\n        Just x ->\n            x\n",
        "Nothing",
    );
}

#[test]
fn missing_literal_catchall_is_reported() {
    missing(
        "f n =\n    case n of\n        0 ->\n            1\n\n        1 ->\n            2\n",
        "_",
    );
    missing(
        "f s =\n    case s of\n        \"a\" ->\n            1\n",
        "_",
    );
}

#[test]
fn list_patterns() {
    ok("f xs =\n    case xs of\n        [] ->\n            0\n\n        x :: rest ->\n            x\n");
    missing(
        "f xs =\n    case xs of\n        [] ->\n            0\n\n        [ x ] ->\n            x\n",
        "::",
    );
    missing(
        "f xs =\n    case xs of\n        x :: rest ->\n            x\n",
        "[]",
    );
}

#[test]
fn nested_patterns_are_checked_deeply() {
    missing(
        "f v =\n    case v of\n        Just (Just x) ->\n            x\n\n        Nothing ->\n            0\n",
        "Just Nothing",
    );
    ok("f v =\n    case v of\n        Just (Just x) ->\n            x\n\n        Just Nothing ->\n            0\n\n        Nothing ->\n            0\n");
}

#[test]
fn tuples_are_checked() {
    missing(
        "f v =\n    case v of\n        ( True, True ) ->\n            1\n\n        ( False, False ) ->\n            2\n",
        "( True, False )",
    );
    ok("f v =\n    case v of\n        ( True, b ) ->\n            1\n\n        ( False, b ) ->\n            2\n");
}

#[test]
fn bools_are_checked() {
    missing(
        "f b =\n    case b of\n        True ->\n            1\n",
        "False",
    );
}

#[test]
fn redundant_branches_are_rejected() {
    let err = check(
        "f m =\n    case m of\n        Just x ->\n            x\n\n        _ ->\n            0\n\n        Nothing ->\n            1\n",
    )
    .unwrap_err();
    assert!(err.contains("redundant"), "got: {}", err);

    let err = check(
        "f n =\n    case n of\n        _ ->\n            0\n\n        1 ->\n            1\n",
    )
    .unwrap_err();
    assert!(err.contains("redundant"), "got: {}", err);
}

#[test]
fn refutable_function_args_are_rejected() {
    let err = check("f (Just x) = x\n\nmain = f (Just 1)").unwrap_err();
    assert!(err.contains("MISSING PATTERNS"), "got: {}", err);
    // Single-constructor unions are fine as argument patterns.
    ok("type Wrapper\n    = Wrapper Int\n\nunwrap (Wrapper n) = n\n\nmain = unwrap (Wrapper 1)");
    ok("f ( a, b ) = a + b\n\nmain = f ( 1, 2 )");
}

#[test]
fn refutable_destructuring_is_rejected() {
    let err = check(
        "main =\n    let\n        (Just x) =\n            Just 1\n    in\n    x\n",
    )
    .unwrap_err();
    assert!(err.contains("MISSING PATTERNS"), "got: {}", err);
}

#[test]
fn wildcards_make_anything_exhaustive() {
    ok("type Big\n    = A\n    | B\n    | C\n    | D\n\nf x =\n    case x of\n        A ->\n            1\n\n        _ ->\n            0\n");
}

#[test]
fn missing_pattern_rendering() {
    // Char literals in the missing-pattern message.
    missing(
        "f c =\n    case c of\n        'a' ->\n            1\n",
        "_",
    );
    // Right-nested cons renders flat; a cons in head position needs parens.
    missing(
        "f xs =\n    case xs of\n        [] ->\n            0\n\n        [ x ] ->\n            x\n",
        "_ :: _ :: _",
    );
    missing(
        "f xss =\n    case xss of\n        [] ->\n            0\n\n        [] :: _ ->\n            1\n",
        "(_ :: _) :: _",
    );
    // Tuple-of-union missing combination renders as a tuple.
    missing(
        "f v =\n    case v of\n        ( Just _, _ ) ->\n            1\n",
        "( Nothing,",
    );
}

#[test]
fn redundant_wildcard_after_complete_match() {
    let err = check(
        "f b =\n    case b of\n        True ->\n            1\n\n        False ->\n            2\n\n        _ ->\n            3\n",
    )
    .unwrap_err();
    assert!(err.contains("redundant"), "got: {}", err);
}

#[test]
fn expressions_inside_every_container_are_checked() {
    // The nitpick walker descends into records, updates, accessors, and
    // tuples; an incomplete case buried in each must still be caught.
    let err = check(
        "base = { field = 0 }\n\nbad m =\n    { base | field = case m of\n            Just n ->\n                n\n    }\n",
    )
    .unwrap_err();
    assert!(err.contains("MISSING PATTERNS"), "got: {}", err);

    let err = check(
        "bad m =\n    ( case m of\n        Just n ->\n            n\n    , (case m of\n        Just k ->\n            k\n      ).x\n    )\n",
    )
    .unwrap_err();
    assert!(err.contains("MISSING PATTERNS"), "got: {}", err);

    let ok_result = check(
        "f m =\n    { a = case m of\n            Just n ->\n                n\n\n            Nothing ->\n                0\n    , b = -(case m of\n            _ ->\n                1\n      )\n    }\n",
    );
    assert!(ok_result.is_ok());
}
