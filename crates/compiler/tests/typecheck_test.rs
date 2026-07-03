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
