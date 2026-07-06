use alm_compiler::{canonicalize, parse, typecheck};

fn check(src: &str) -> Result<(), Vec<String>> {
    let module = parse::parse_module(src)
        .map_err(|e| vec![format!("parse error: {}", e.message)])?;
    let canonical = canonicalize::canonicalize(&module)
        .map_err(|es| es.into_iter().map(|e| format!("canonicalize error: {}", e.message)).collect::<Vec<_>>())?;
    typecheck::check(&canonical)
        .map_err(|es| es.into_iter().map(|e| e.message).collect())
}

fn check_ok(src: &str) {
    if let Err(errors) = check(src) {
        panic!("expected well-typed, got:\n{}", errors.join("\n---\n"));
    }
}

fn check_err(src: &str) {
    if check(src).is_ok() {
        panic!("expected a type error for:\n{}", src);
    }
}

const HEADER: &str = "module Test exposing (..)\n\n";

fn ok(body: &str) {
    check_ok(&format!("{}{}", HEADER, body));
}

fn err(body: &str) {
    check_err(&format!("{}{}", HEADER, body));
}

#[test]
fn literals_and_arithmetic() {
    ok("x = 1 + 2 * 3\n");
    ok("x = 1.5 + 2.5\n");
    ok("x : Int\nx = 1 + 2\n");
    ok("x : Float\nx = 1 + 2\n"); // number literals default to any number type
    err("x = 1 + \"two\"\n");
    err("x : Int\nx = 1.5\n");
    err("x : String\nx = 1\n");
}

#[test]
fn int_float_do_not_mix() {
    err("x = 1.5 + (1 // 2)\n"); // Float + Int
    ok("x = toFloat (1 // 2) + 1.5\n");
    err("x = 1 / 2 + 3 // 4\n");
}

#[test]
fn strings_and_append() {
    ok("x = \"a\" ++ \"b\"\n");
    ok("x = [ 1 ] ++ [ 2 ]\n");
    err("x = \"a\" ++ [ 1 ]\n");
    err("x = 1 ++ 2\n"); // numbers are not appendable
}

#[test]
fn comparisons() {
    ok("x = 1 < 2\n");
    ok("x = \"a\" < \"b\"\n");
    err("x = True < False\n"); // Bool is not comparable
    ok("x = ( 1, \"a\" ) < ( 2, \"b\" )\n"); // tuples of comparables
}

#[test]
fn conditionals() {
    ok("x = if True then 1 else 2\n");
    err("x = if 1 then 2 else 3\n");
    err("x = if True then 1 else \"two\"\n");
}

#[test]
fn functions_and_application() {
    ok("f x = x + 1\ny = f 2\n");
    err("f x = x + 1\ny = f \"two\"\n");
    ok("f : Int -> Int -> Int\nf a b = a + b\ny = f 1 2\n");
    err("f : Int -> Int\nf a = a\ny = f 1 2\n"); // too many args
    ok("apply : (a -> b) -> a -> b\napply f x = f x\n");
}

#[test]
fn annotation_must_match() {
    err("f : Int -> Int\nf x = x ++ \"!\"\n");
    err("f : a -> a\nf x = x + 1\n"); // rigid var cannot become number
    ok("f : a -> a\nf x = x\n");
    err("f : a -> b\nf x = x\n"); // cannot promise b from a
}

#[test]
fn polymorphism() {
    ok("id x = x\na = id 1\nb = id \"s\"\n");
    ok("pair x y = ( x, y )\np = pair 1 \"a\"\n");
}

#[test]
fn let_polymorphism() {
    ok("x =\n    let\n        id v = v\n    in\n    ( id 1, id \"a\" )\n");
    ok("x =\n    let\n        n = 1\n        m = n + 1\n    in\n    m\n");
    err("x =\n    let\n        f v = v + 1\n    in\n    f \"a\"\n");
}

#[test]
fn let_annotations() {
    ok("x =\n    let\n        n : Int\n        n = 1\n    in\n    n\n");
    err("x =\n    let\n        n : String\n        n = 1\n    in\n    n\n");
}

#[test]
fn lists_are_homogeneous() {
    ok("x = [ 1, 2, 3 ]\n");
    err("x = [ 1, \"two\" ]\n");
    ok("x = 1 :: 2 :: []\n");
    err("x = 1 :: \"two\" :: []\n");
}

#[test]
fn custom_types() {
    ok("type Shape\n    = Circle Float\n    | Square Float\n\narea s =\n    case s of\n        Circle r ->\n            3.14 * r * r\n\n        Square w ->\n            w * w\n");
    err("type Shape\n    = Circle Float\n\nbad = Circle \"big\"\n");
    ok("type Tree a\n    = Leaf\n    | Node (Tree a) a (Tree a)\n\ninsert : comparable -> Tree comparable -> Tree comparable\ninsert x tree =\n    case tree of\n        Leaf ->\n            Node Leaf x Leaf\n\n        Node left y right ->\n            if x < y then\n                Node (insert x left) y right\n            else\n                Node left y (insert x right)\n");
}

#[test]
fn maybe_and_result() {
    ok("x = Just 1\ny = Nothing\nz = Maybe.withDefault 0 x\n");
    ok("safeDiv : Int -> Int -> Maybe Int\nsafeDiv a b =\n    if b == 0 then\n        Nothing\n    else\n        Just (a // b)\n");
    err("x : Maybe String\nx = Just 1\n");
    ok("r : Result String Int\nr = Ok 1\n");
}

#[test]
fn case_branches_must_agree() {
    err("x =\n    case Just 1 of\n        Just n ->\n            n\n\n        Nothing ->\n            \"zero\"\n");
    err("x =\n    case Just 1 of\n        Just n ->\n            n\n\n        \"nope\" ->\n            0\n");
}

#[test]
fn records() {
    ok("p = { x = 1, y = 2 }\nsum = p.x + p.y\n");
    ok("getX : { r | x : Int } -> Int\ngetX r = r.x\na = getX { x = 1, y = 2 }\nb = getX { x = 3 }\n");
    err("getX r = r.x + 1\nbad = getX { y = 2 }\n");
    ok("move p = { p | x = p.x + 1 }\nq = move { x = 1, y = 0 }\n");
    err("bad = { x = 1 }.y\n");
    ok("f = .name\nn = f { name = \"alm\" }\n");
    err("update p = { p | x = \"s\" }\nbad = update { x = 1 }\n");
}

#[test]
fn record_type_annotations() {
    ok("type alias Point =\n    { x : Float, y : Float }\n\norigin : Point\norigin = { x = 0, y = 0 }\n");
    err("type alias Point =\n    { x : Float, y : Float }\n\nbad : Point\nbad = { x = 0 }\n");
}

#[test]
fn recursion() {
    ok("fact n =\n    if n <= 1 then\n        1\n    else\n        n * fact (n - 1)\n");
    ok("isEven : Int -> Bool\nisEven n =\n    if n == 0 then\n        True\n    else\n        isOdd (n - 1)\n\nisOdd : Int -> Bool\nisOdd n =\n    if n == 0 then\n        False\n    else\n        isEven (n - 1)\n");
    err("x = x + 1\n"); // self-recursive value
}

#[test]
fn pipelines_and_composition() {
    ok("double n = n * 2\nx = [ 1, 2, 3 ] |> List.map double |> List.length\n");
    ok("f = String.fromInt >> String.length\nn = f 12\n");
    err("x = 1 |> String.length\n");
}

#[test]
fn destructuring() {
    ok("x =\n    let\n        ( a, b ) =\n            ( 1, \"two\" )\n    in\n    a\n");
    ok("dist ( x, y ) = sqrt (x * x + y * y)\nd = dist ( 3.0, 4.0 )\n");
    err("x =\n    let\n        ( a, b ) =\n            ( 1, 2, 3 )\n    in\n    a\n");
}

#[test]
fn unbound_variables_are_caught() {
    err("x = missingThing + 1\n");
    err("x = List.notAFunction 1\n");
}

#[test]
fn tuples() {
    ok("x : ( Int, String, Char )\nx = ( 1, \"a\", 'c' )\n");
    err("x : ( Int, String )\nx = ( 1, 2 )\n");
}

#[test]
fn debug_helpers() {
    ok("x = Debug.toString [ 1, 2 ]\n");
    ok("x = Debug.log \"value\" 42\n");
}

#[test]
fn infinite_types_are_rejected() {
    err("f x = f\n");
    err("bad g = g g\n");
}

// COVERAGE: constrained type variables (rigid and flexible supers)

#[test]
fn rigid_super_variables() {
    // A rigid `number` satisfies number-constrained operations.
    ok("f : number -> number\nf x = x + x\n");
    // A rigid `comparable` satisfies comparison operators.
    ok("f : comparable -> comparable -> Bool\nf a b = a < b\n");
    // A rigid `appendable` satisfies ++ but not +.
    ok("f : appendable -> appendable -> appendable\nf a b = a ++ b\n");
    err("f : appendable -> appendable\nf x = x + x\n");
    // A rigid `number` is comparable.
    ok("f : number -> number -> Bool\nf a b = a < b\n");
    // A plain rigid var satisfies nothing.
    err("f : a -> a\nf x = x ++ x\n");
}

#[test]
fn super_combinations() {
    // appendable + comparable = compappend (Strings and Lists qualify).
    ok("f x y = ( x ++ y, x < y )\ng = f \"a\" \"b\"\n");
    ok("f x y = ( x ++ y, x < y )\ng = f [ 1 ] [ 2 ]\n");
    // number + appendable has no intersection.
    err("f x = ( x ++ x, x + x )\n");
    // number + comparable = number.
    ok("f x y = ( x + y, x < y )\ng = f 1 2\n");
    err("f x y = ( x + y, x < y )\ng = f \"a\" \"b\"\n");
}

#[test]
fn record_updates_are_checked() {
    err("notARecord = 5\n\nbad = { notARecord | x = 1 }\n");
    // The rendered error includes an extensible record type.
    err("f r = { r | x = r.x + 1, missing : Int }\n");
    err("addField r = { r | x = 1 }\n\nbad = addField { y = 2 }\n");
}

#[test]
fn partial_generalization_keeps_outer_vars_mono() {
    // `g` is polymorphic in its own argument but shares `x`'s type var
    // with the enclosing scope.
    ok("f x =\n    let\n        g y =\n            ( x, y )\n    in\n    ( g 1, g \"s\" )\n");
    // Using g to force x to two different types must fail.
    err("f x =\n    let\n        g y =\n            ( x + y, y )\n    in\n    ( g 1, g \"s\" )\n");
}

#[test]
fn error_rendering_covers_type_shapes() {
    // Three-element tuples in the rendered mismatch.
    err("x : ( Int, Int, Int )\nx = ( 1, 2, \"3\" )\n");
    // Function types as arguments get parenthesized in messages.
    err("apply : (Int -> Int) -> Int\napply f = f 1\n\nbad = apply 5\n");
    // Extensible records render with their row variable.
    err("getX : { r | x : Int } -> Int\ngetX r = r.x\n\nbad = getX \"not a record\"\n");
    // Unit in messages.
    err("f : () -> Int\nf _ = 1\n\nbad = f 0\n");
}

#[test]
fn many_type_variables_generalize() {
    // More than 26 fresh variables exercises the name generator overflow
    // (a..z then wrapping).
    ok("f a b c d e g h i j k l m n o p q r s t u v w x y z aa ab =\n    { f1 = a, f2 = b, f3 = c, f4 = d, f5 = e, f6 = g, f7 = h, f8 = i, f9 = j, f10 = k, f11 = l, f12 = m, f13 = n, f14 = o, f15 = p, f16 = q, f17 = r, f18 = s, f19 = t, f20 = u, f21 = v, f22 = w, f23 = x, f24 = y, f25 = z, f26 = aa, f27 = ab }\n");
}

#[test]
fn mutually_recursive_with_annotations() {
    ok("even : Int -> Bool\neven n =\n    if n == 0 then\n        True\n    else\n        odd (n - 1)\n\nodd n =\n    if n == 0 then\n        False\n    else\n        even (n - 1)\n");
    // An annotated member of a recursive group whose body violates the
    // annotation.
    err("even : Int -> String\neven n =\n    if n == 0 then\n        True\n    else\n        odd (n - 1)\n\nodd n =\n    if n == 0 then\n        False\n    else\n        even (n - 1)\n");
}

#[test]
fn pattern_type_errors() {
    err("f xs =\n    case xs of\n        [ a ] ->\n            a\n\n        x :: rest ->\n            x + String.length rest\n\n        [] ->\n            0\n");
    err("f p =\n    case p of\n        { name } ->\n            name ++ \"!\"\n\ng = f { name = 5 }\n");
    err("f v =\n    case v of\n        ( a, b ) ->\n            a\n\ng = f 5\n");
}

#[test]
fn as_patterns_typecheck() {
    ok("f (( a, b ) as pair) = ( a + b, pair )\n\nx = f ( 1, 2 )\n");
    ok("g v =\n    case v of\n        (Just n) as whole ->\n            ( n, whole )\n\n        Nothing ->\n            ( 0, Nothing )\n");
}

#[test]
fn char_and_string_patterns_typecheck() {
    ok("f c =\n    case c of\n        'x' ->\n            1\n\n        _ ->\n            0\n\ng = f 'y'\n");
    err("f c =\n    case c of\n        'x' ->\n            1\n\n        _ ->\n            0\n\ng = f \"not a char\"\n");
    ok("h s =\n    case s of\n        \"lit\" ->\n            1\n\n        _ ->\n            0\n");
}

#[test]
fn rigid_vars_flow_into_inner_lets() {
    // The inner unannotated definition's type mentions the outer rigid
    // `number`; generalization must keep it shared.
    ok("f : number -> number\nf x =\n    let\n        doubled =\n            x + x\n    in\n    doubled\n");
    ok("f : comparable -> comparable -> comparable\nf a b =\n    let\n        smaller =\n            if a < b then\n                a\n            else\n                b\n    in\n    smaller\n");
}

#[test]
fn annotation_variable_names_are_reused_in_errors() {
    // Rendered messages use the annotation's own variable names.
    err("f : thing -> thing\nf x = x + 1\n");
    err("swap : ( first, second ) -> ( second, first )\nswap ( a, b ) = ( a, b )\n");
}

#[test]
fn list_pattern_element_mismatch() {
    err("f xs =\n    case xs of\n        [ a, b ] ->\n            a + b\n\n        _ ->\n            0\n\ng = f [ \"x\" ]\n");
    err("f v =\n    case v of\n        x :: _ ->\n            x + 1\n\n        [] ->\n            0\n\ng = f [ \"s\" ]\n");
}

#[test]
fn extensible_record_alias_applied_with_concrete_record() {
    // Applying an extensible-record alias `{ base | ... }` with a concrete
    // record for `base` must flatten it: `Keyframes {}` becomes the closed
    // record `{ keyframes : (), value : String }`, so a body producing that
    // closed record type-checks. (Regression: rtfeldman/elm-css keyframes.)
    ok("type alias Keyframes compatible =\n\
        \x20   { compatible | keyframes : (), value : String }\n\
        \n\
        make : Keyframes {}\n\
        make =\n    { keyframes = (), value = \"x\" }\n");
    // Applying it with another record extends the field set.
    ok("type alias WithK base =\n\
        \x20   { base | k : Int }\n\
        \n\
        make : WithK { v : String }\n\
        make =\n    { k = 1, v = \"x\" }\n");
}

#[test]
fn wildcard_import_exposes_builtin_types_and_ctors() {
    // `exposing (..)` on a builtin module must bring its type aliases
    // (Svg.Svg) and its union constructors (Time.Weekday) into scope, not
    // just its values. (Regression: EdutainmentLIVE/elm-dropdown, which
    // does `import Svg exposing (..)` and annotates with `Svg msg`.)
    ok("import Svg exposing (..)\n\nthing : Svg msg\nthing =\n    Svg.text \"x\"\n");
    ok("import Time exposing (..)\n\nday : Weekday\nday =\n    Mon\n");
}

#[test]
fn record_alias_constructor_follows_alias_chain() {
    // `type alias Point = Coord` where `Coord` is a record alias: using
    // `Point x y` as a constructor must chase the chain to the record and
    // build a two-argument constructor. (Regression: Bractlet/elm-plot's
    // `type alias Point = Draw.Point`.)
    ok("type alias Coord =\n    { x : Float, y : Float }\n\
        \n\
        type alias Point =\n    Coord\n\
        \n\
        p : Point\n\
        p =\n    Point 1.0 2.0\n");
}

#[test]
fn annotated_param_field_read_in_generalizing_let() {
    // Regression: a record parameter whose field returns a function
    // (`add : a -> Interpolator a`, i.e. `a -> Float -> a`) is read inside a
    // `let` binding that generalizes mid-body. The parameter must be
    // constrained by the annotation *before* the body is inferred; otherwise
    // the field type is still an unconstrained flex when the `let`
    // generalizes, so the element type collapses — `add` was inferred as
    // `List a` instead of `List (Float -> a)`, yielding a spurious mismatch
    // at `interpolator :: add`. Distilled from gampleman/elm-visualization
    // `Interpolation.list`.
    ok(r#"import Dict exposing (Dict)


type alias Interpolator a =
    Float -> a


list :
    { add : a -> Interpolator a
    , change : a -> a -> Interpolator a
    , id : a -> comparable
    }
    -> List a
    -> List (Interpolator a)
list config from =
    let
        fromIds =
            Dict.fromList (List.indexedMap (\idx a -> ( config.id a, ( idx, a ) )) from)

        removals : Dict comparable ( Int, a )
        removals =
            fromIds

        folder : comparable -> ( Int, a ) -> List (Interpolator a) -> List (Interpolator a)
        folder id ( idx, a ) result =
            let
                add =
                    Dict.get idx removals
                        |> Maybe.map (\( _, x ) -> config.add x)
                        |> Maybe.map List.singleton
                        |> Maybe.withDefault []

                interpolator =
                    config.change a a
            in
            result ++ (interpolator :: add)
    in
    Dict.foldl folder [] fromIds
"#);
}

#[test]
fn let_helper_flex_var_not_conflated_with_free_rigid() {
    // Regression: `consIndexIf`'s `index` parameter is a flexible variable
    // that gets generalized while the enclosing annotation's rigid `a` is a
    // free variable. Generalization must not hand the quantified variable
    // the same *name* as the free rigid variable — `instantiate` keys its
    // substitution map by name, so a collision would conflate the two and
    // pin `index` to the rigid `a` (rejecting this valid code). Distilled
    // from elm-community/list-extra `findIndices`.
    ok(r#"indexedFoldr : (Int -> a -> b -> b) -> b -> List a -> b
indexedFoldr _ acc _ =
    acc


findIndices : (a -> Bool) -> List a -> List Int
findIndices predicate =
    let
        consIndexIf index x acc =
            if predicate x then
                index :: acc

            else
                acc
    in
    indexedFoldr consIndexIf []
"#);
}

#[test]
fn generalization_does_not_conflate_distinct_free_vars() {
    // Regression: two distinct captured (env-free) type variables could both
    // carry the stored name "a" (each from a separate `List.head : List a ->
    // Maybe a` instantiation). Generalization keys its free-variable map by
    // name, so seeding both onto one name conflated them at instantiation,
    // unifying `List Char` with `List Token` — a spurious TYPE MISMATCH that
    // appeared nondeterministically depending on HashSet iteration order.
    // Distilled from abadi199/elm-input-extra `MaskedInput.Pattern.isValid`.
    // Loop to defeat HashMap seed randomization.
    let body = r#"type Token = Input | Other Char

scan : List Token -> List Char -> String -> String
scan tokens input value =
    let
        maybeToken = List.head tokens
        maybeInputChar = List.head input
        parseToken token inputChar =
            case token of
                Input ->
                    scan (Maybe.withDefault [] (List.tail tokens)) (Maybe.withDefault [] (List.tail input)) (value ++ String.fromChar inputChar)
                Other other ->
                    if other == inputChar then
                        scan (Maybe.withDefault [] (List.tail tokens)) (Maybe.withDefault [] (List.tail input)) value
                    else
                        String.fromList input
    in
    case maybeToken of
        Nothing -> value
        Just token -> maybeInputChar |> Maybe.map (parseToken token) |> Maybe.withDefault value

isValid : String -> List Token -> Bool
isValid value tokens =
    let
        scanIsValid unscannedCharacters unscannedTokens =
            let
                currentToken = List.head unscannedTokens
                currentCharacter = List.head unscannedCharacters
                tailTokens = List.tail unscannedTokens |> Maybe.withDefault []
                tailCharacters = List.tail unscannedCharacters |> Maybe.withDefault []
                isCharacterEmpty = List.isEmpty unscannedCharacters
                isTokenEmpty = List.isEmpty unscannedTokens
            in
            if isCharacterEmpty then True
            else
                case currentToken of
                    Just Input -> scanIsValid tailCharacters tailTokens
                    Just (Other other) ->
                        currentCharacter
                            |> Maybe.map ((==) other)
                            |> Maybe.map (\isMatch -> if isMatch then scanIsValid tailCharacters tailTokens else False)
                            |> Maybe.withDefault False
                    Nothing -> False
    in
    scanIsValid (String.toList value) tokens
"#;
    for _ in 0..100 {
        ok(body);
    }
}

#[test]
fn glsl_shader_typechecks() {
    // A `[glsl|...|]` literal is accepted and takes a flexible type, so it
    // unifies with whatever `WebGL.Shader ...` annotation or use site pins it.
    ok("vertexShader =\n    [glsl| attribute vec3 position; uniform mat4 m; |]\n");
    // Usable where any value is expected (here, inside a tuple).
    ok("pair =\n    ( [glsl| attribute vec3 a; |], 1 )\n");
}
