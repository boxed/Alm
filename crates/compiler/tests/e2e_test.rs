//! End-to-end tests: compile Elm source to JavaScript and execute it with
//! node. Every fixture module is named `Test` and defines `main : String`;
//! the harness prints it and asserts on the output.

use std::process::Command;

fn run(body: &str) -> String {
    let source = format!("module Test exposing (..)\n\n{}", body);
    let javascript = match alm_compiler::compile(&source) {
        Ok(js) => js,
        Err(reports) => panic!(
            "compilation failed:\n{}",
            reports
                .iter()
                .map(|r| r.render("Test.elm", &source))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };

    let dir = std::env::temp_dir().join(format!(
        "alm-e2e-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let js_path = dir.join("test.js");
    std::fs::write(&js_path, &javascript).unwrap();

    let output = Command::new("node")
        .arg("-e")
        .arg(format!(
            "console.log(require({:?})['Test']['main']);",
            js_path.to_str().unwrap()
        ))
        .output()
        .expect("failed to run node");

    if !output.status.success() {
        panic!(
            "node failed:\n{}\n\ngenerated JS:\n{}",
            String::from_utf8_lossy(&output.stderr),
            javascript
        );
    }
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

#[test]
fn arithmetic() {
    assert_eq!(run("main = String.fromInt (1 + 2 * 3 - 4)"), "3");
    assert_eq!(run("main = String.fromInt (10 // 3)"), "3");
    assert_eq!(run("main = String.fromInt (2 ^ 10)"), "1024");
    assert_eq!(run("main = String.fromFloat (10 / 4)"), "2.5");
    assert_eq!(run("main = String.fromInt (modBy 3 -1)"), "2");
    assert_eq!(run("main = String.fromInt (negate 5)"), "-5");
}

#[test]
fn precedence_and_associativity() {
    assert_eq!(run("main = String.fromInt (10 - 3 - 2)"), "5"); // left assoc
    assert_eq!(run("main = String.fromFloat (2 ^ 3 ^ 2)"), "512"); // right assoc
    assert_eq!(
        run("main = String.fromInt (1 + 2 * 3 ^ 2)"),
        "19"
    );
}

#[test]
fn recursion() {
    assert_eq!(
        run("fact n =\n    if n <= 1 then\n        1\n    else\n        n * fact (n - 1)\n\nmain = String.fromInt (fact 10)"),
        "3628800"
    );
    assert_eq!(
        run("fib n =\n    if n < 2 then\n        n\n    else\n        fib (n - 1) + fib (n - 2)\n\nmain = String.fromInt (fib 20)"),
        "6765"
    );
}

#[test]
fn mutual_recursion() {
    assert_eq!(
        run("isEven n =\n    if n == 0 then\n        True\n    else\n        isOdd (n - 1)\n\nisOdd n =\n    if n == 0 then\n        False\n    else\n        isEven (n - 1)\n\nmain = Debug.toString (isEven 10)"),
        "True"
    );
}

#[test]
fn lists() {
    assert_eq!(
        run("main = Debug.toString (List.map (\\x -> x * 2) [ 1, 2, 3 ])"),
        "[2,4,6]"
    );
    assert_eq!(
        run("main = String.fromInt (List.sum (List.range 1 100))"),
        "5050"
    );
    assert_eq!(
        run("main = Debug.toString (List.filter (\\x -> modBy 2 x == 0) (List.range 1 10))"),
        "[2,4,6,8,10]"
    );
    assert_eq!(
        run("main = String.fromInt (List.foldl (+) 0 [ 1, 2, 3, 4 ])"),
        "10"
    );
    assert_eq!(
        run("main = Debug.toString (List.sort [ 3, 1, 2 ])"),
        "[1,2,3]"
    );
    assert_eq!(run("main = Debug.toString (1 :: 2 :: [])"), "[1,2]");
    assert_eq!(
        run("main = Debug.toString ([ 1, 2 ] ++ [ 3 ])"),
        "[1,2,3]"
    );
}

#[test]
fn strings() {
    assert_eq!(run("main = \"Hello, \" ++ \"World!\""), "Hello, World!");
    assert_eq!(
        run("main = String.join \", \" (List.map String.fromInt [ 1, 2, 3 ])"),
        "1, 2, 3"
    );
    assert_eq!(run("main = String.toUpper \"alm\""), "ALM");
    assert_eq!(
        run("main = String.repeat 3 \"ab\""),
        "ababab"
    );
    assert_eq!(
        run("main = Debug.toString (String.toInt \"42\")"),
        "Just 42"
    );
    assert_eq!(
        run("main = Debug.toString (String.toInt \"4x\")"),
        "Nothing"
    );
}

#[test]
fn case_expressions() {
    assert_eq!(
        run("describe n =\n    case n of\n        0 ->\n            \"zero\"\n\n        1 ->\n            \"one\"\n\n        _ ->\n            \"many\"\n\nmain = describe 1"),
        "one"
    );
    assert_eq!(
        run("len list =\n    case list of\n        [] ->\n            0\n\n        _ :: rest ->\n            1 + len rest\n\nmain = String.fromInt (len [ 1, 2, 3 ])"),
        "3"
    );
    assert_eq!(
        run("first pair =\n    case pair of\n        ( a, _ ) ->\n            a\n\nmain = first ( \"win\", 2 )"),
        "win"
    );
}

#[test]
fn custom_types() {
    let tree = "type Tree\n    = Leaf\n    | Node Tree Int Tree\n\ninsert : Int -> Tree -> Tree\ninsert x tree =\n    case tree of\n        Leaf ->\n            Node Leaf x Leaf\n\n        Node left y right ->\n            if x < y then\n                Node (insert x left) y right\n            else if x > y then\n                Node left y (insert x right)\n            else\n                tree\n\ntoList : Tree -> List Int\ntoList tree =\n    case tree of\n        Leaf ->\n            []\n\n        Node left x right ->\n            toList left ++ (x :: toList right)\n\nmain = Debug.toString (toList (List.foldl insert Leaf [ 5, 3, 8, 1, 4, 9, 2, 7, 6 ]))";
    assert_eq!(run(tree), "[1,2,3,4,5,6,7,8,9]");
}

#[test]
fn maybe_and_result() {
    assert_eq!(
        run("safeDiv a b =\n    if b == 0 then\n        Nothing\n    else\n        Just (a // b)\n\nmain = Debug.toString (safeDiv 10 2)"),
        "Just 5"
    );
    assert_eq!(
        run("main = String.fromInt (Maybe.withDefault 0 (Just 42))"),
        "42"
    );
    assert_eq!(
        run("main = Debug.toString (Maybe.map (\\x -> x + 1) (Just 1))"),
        "Just 2"
    );
    assert_eq!(
        run("main = Debug.toString (Result.map String.length (Ok \"four\"))"),
        "Ok 4"
    );
}

#[test]
fn let_expressions() {
    assert_eq!(
        run("main =\n    let\n        x =\n            3\n\n        y =\n            4\n    in\n    String.fromFloat (sqrt (toFloat (x * x + y * y)))"),
        "5"
    );
    // Definitions used before they are defined (dependency sorting).
    assert_eq!(
        run("main =\n    let\n        a =\n            b + 1\n\n        b =\n            2\n    in\n    String.fromInt a"),
        "3"
    );
    // Destructuring.
    assert_eq!(
        run("main =\n    let\n        ( a, b ) =\n            ( \"al\", \"m\" )\n    in\n    a ++ b"),
        "alm"
    );
    // Recursive let function.
    assert_eq!(
        run("main =\n    let\n        go n acc =\n            if n == 0 then\n                acc\n            else\n                go (n - 1) (acc + n)\n    in\n    String.fromInt (go 100 0)"),
        "5050"
    );
}

#[test]
fn records() {
    assert_eq!(
        run("point = { x = 3, y = 4 }\n\nmain = String.fromInt (point.x + point.y)"),
        "7"
    );
    assert_eq!(
        run("move p = { p | x = p.x + 10 }\n\nmain = Debug.toString (move { x = 1, y = 2 })"),
        "{ x = 11, y = 2 }"
    );
    assert_eq!(
        run("main = String.concat (List.map .name [ { name = \"a\" }, { name = \"b\" } ])"),
        "ab"
    );
    assert_eq!(
        run("getX : { r | x : Int } -> Int\ngetX r = r.x\n\nmain = String.fromInt (getX { x = 7, y = 0 } + getX { x = 3 })"),
        "10"
    );
}

#[test]
fn higher_order_functions() {
    assert_eq!(
        run("apply f x = f x\n\nmain = String.fromInt (apply (\\n -> n * 2) 21)"),
        "42"
    );
    // Partial application / currying.
    assert_eq!(
        run("add a b = a + b\n\nincrement = add 1\n\nmain = String.fromInt (increment 41)"),
        "42"
    );
    // Operator sections.
    assert_eq!(
        run("main = String.fromInt (List.foldr (+) 0 [ 1, 2, 3 ])"),
        "6"
    );
    // Composition.
    assert_eq!(
        run("f = String.fromInt >> String.length\n\nmain = String.fromInt (f 12345)"),
        "5"
    );
}

#[test]
fn pipelines() {
    assert_eq!(
        run("main =\n    List.range 1 10\n        |> List.filter (\\n -> modBy 2 n == 0)\n        |> List.map (\\n -> n * n)\n        |> List.sum\n        |> String.fromInt"),
        "220"
    );
    assert_eq!(
        run("main = String.fromInt <| 1 + 2"),
        "3"
    );
}

#[test]
fn equality_is_structural() {
    assert_eq!(
        run("main = Debug.toString ([ 1, 2 ] == [ 1, 2 ])"),
        "True"
    );
    assert_eq!(
        run("main = Debug.toString ({ a = 1 } == { a = 2 })"),
        "False"
    );
    assert_eq!(
        run("main = Debug.toString (Just [ ( 1, \"a\" ) ] == Just [ ( 1, \"a\" ) ])"),
        "True"
    );
}

#[test]
fn comparisons_work_on_structures() {
    assert_eq!(
        run("main = Debug.toString (compare ( 1, \"b\" ) ( 1, \"a\" ))"),
        "GT"
    );
    assert_eq!(
        run("main = Debug.toString (List.sort [ ( 2, \"a\" ), ( 1, \"b\" ) ])"),
        "[(1,\"b\"),(2,\"a\")]"
    );
    assert_eq!(
        run("main = Debug.toString (List.maximum [ 3, 1, 4, 1, 5 ])"),
        "Just 5"
    );
}

#[test]
fn negation_and_unary_minus() {
    assert_eq!(run("main = String.fromInt -5"), "-5");
    assert_eq!(
        run("f x = x + 1\n\nmain = String.fromInt (f -3)"),
        "-2"
    );
    assert_eq!(
        run("x = 10\n\nmain = String.fromInt (5 - x)"),
        "-5"
    );
}

#[test]
fn chars() {
    assert_eq!(run("main = String.fromChar 'x'"), "x");
    assert_eq!(
        run("main = String.fromInt (Char.toCode 'A')"),
        "65"
    );
    assert_eq!(
        run("main = Debug.toString (String.toList \"ab\")"),
        "[\"a\",\"b\"]"
    );
}

#[test]
fn shadowed_names_in_branches() {
    assert_eq!(
        run("f x =\n    case x of\n        Just y ->\n            y\n\n        Nothing ->\n            0\n\nmain = String.fromInt (f (Just 3) + f Nothing)"),
        "3"
    );
}

#[test]
fn nested_patterns() {
    assert_eq!(
        run("f v =\n    case v of\n        Just ( x, [ y, z ] ) ->\n            x + y + z\n\n        _ ->\n            0\n\nmain = String.fromInt (f (Just ( 1, [ 2, 3 ] )))"),
        "6"
    );
    assert_eq!(
        run("f v =\n    case v of\n        ( Just x ) as whole ->\n            Debug.toString whole ++ \"/\" ++ String.fromInt x\n\n        Nothing ->\n            \"none\"\n\nmain = f (Just 9)"),
        "Just 9/9"
    );
}

#[test]
fn type_aliases_work() {
    assert_eq!(
        run("type alias Person =\n    { name : String, age : Int }\n\ngreet : Person -> String\ngreet person =\n    \"Hi \" ++ person.name\n\nmain = greet { name = \"Anders\", age = 40 }"),
        "Hi Anders"
    );
}

#[test]
fn booleans() {
    assert_eq!(
        run("main = Debug.toString (True && not False || xor True True)"),
        "True"
    );
    assert_eq!(
        run("main = Debug.toString (1 < 2 && 2 <= 2 && 3 > 2 && 3 >= 3 && 1 /= 2)"),
        "True"
    );
}

#[test]
fn tuples() {
    assert_eq!(
        run("main = String.fromInt (Tuple.first ( 1, \"a\" ) + Tuple.second ( \"b\", 2 ))"),
        "3"
    );
    assert_eq!(
        run("main = Debug.toString (List.unzip [ ( 1, \"a\" ), ( 2, \"b\" ) ])"),
        "([1,2],[\"a\",\"b\"])"
    );
}

#[test]
fn tail_calls_do_not_grow_the_stack() {
    // One million iterations would overflow the stack without TCO.
    assert_eq!(
        run("count : Int -> Int -> Int\ncount acc n =\n    if n == 0 then\n        acc\n    else\n        count (acc + 1) (n - 1)\n\nmain = String.fromInt (count 0 1000000)"),
        "1000000"
    );
    // Tail recursion through case branches and let bodies.
    assert_eq!(
        run("sumList : Int -> List Int -> Int\nsumList acc xs =\n    case xs of\n        [] ->\n            acc\n\n        x :: rest ->\n            let\n                newAcc =\n                    acc + x\n            in\n            sumList newAcc rest\n\nmain = String.fromInt (sumList 0 (List.range 1 100000))"),
        "5000050000"
    );
    // `f = \\x -> ...` style definitions also get the optimization.
    assert_eq!(
        run("loop : Int -> Int\nloop =\n    \\n ->\n        if n > 0 then\n            loop (n - 1)\n        else\n            n\n\nmain = String.fromInt (loop 500000)"),
        "0"
    );
}

#[test]
fn non_tail_recursion_still_works() {
    // The argument swap must use temporaries: `swap a b = swap b a` style.
    assert_eq!(
        run("gcd : Int -> Int -> Int\ngcd a b =\n    if b == 0 then\n        a\n    else\n        gcd b (modBy b a)\n\nmain = String.fromInt (gcd 1071 462)"),
        "21"
    );
}

#[test]
fn dicts() {
    assert_eq!(
        run("main = Debug.toString (Dict.get \"b\" (Dict.fromList [ ( \"a\", 1 ), ( \"b\", 2 ) ]))"),
        "Just 2"
    );
    assert_eq!(
        run("main =\n    Dict.empty\n        |> Dict.insert 3 \"three\"\n        |> Dict.insert 1 \"one\"\n        |> Dict.insert 2 \"two\"\n        |> Dict.remove 2\n        |> Debug.toString"),
        "Dict.fromList [(1,\"one\"),(3,\"three\")]"
    );
    assert_eq!(
        run("main = Debug.toString (Dict.update \"k\" (Maybe.map (\\n -> n + 1)) (Dict.singleton \"k\" 1))"),
        "Dict.fromList [(\"k\",2)]"
    );
    assert_eq!(
        run("counts words =\n    List.foldl (\\w d -> Dict.update w (\\m -> Just (Maybe.withDefault 0 m + 1)) d) Dict.empty words\n\nmain = Debug.toString (counts [ \"a\", \"b\", \"a\" ])"),
        "Dict.fromList [(\"a\",2),(\"b\",1)]"
    );
    assert_eq!(
        run("main = Debug.toString (Dict.union (Dict.fromList [ ( 1, \"L\" ) ]) (Dict.fromList [ ( 1, \"R\" ), ( 2, \"R\" ) ]))"),
        "Dict.fromList [(1,\"L\"),(2,\"R\")]"
    );
}

#[test]
fn sets() {
    assert_eq!(
        run("main = Debug.toString (Set.toList (Set.fromList [ 3, 1, 2, 1, 3 ]))"),
        "[1,2,3]"
    );
    assert_eq!(
        run("main = Debug.toString (Set.member 2 (Set.fromList [ 1, 2 ]))"),
        "True"
    );
    assert_eq!(
        run("main = Debug.toString (Set.toList (Set.intersect (Set.fromList [ 1, 2, 3 ]) (Set.fromList [ 2, 3, 4 ])))"),
        "[2,3]"
    );
}

#[test]
fn arrays() {
    assert_eq!(
        run("main = Debug.toString (Array.get 1 (Array.fromList [ 10, 20, 30 ]))"),
        "Just 20"
    );
    assert_eq!(
        run("main = Debug.toString (Array.toList (Array.set 0 99 (Array.initialize 3 (\\i -> i * i))))"),
        "[99,1,4]"
    );
    assert_eq!(
        run("main = String.fromInt (Array.foldl (+) 0 (Array.push 4 (Array.fromList [ 1, 2, 3 ])))"),
        "10"
    );
}

#[test]
fn bitwise() {
    assert_eq!(
        run("main = String.fromInt (Bitwise.and 12 10)"),
        "8"
    );
    assert_eq!(
        run("main = String.fromInt (Bitwise.shiftLeftBy 4 1 + Bitwise.shiftRightZfBy 1 6)"),
        "19"
    );
}

#[test]
fn string_extras() {
    assert_eq!(
        run("main = Debug.toString (String.uncons \"abc\")"),
        "Just (\"a\",\"bc\")"
    );
    assert_eq!(
        run("main = Debug.toString (String.indexes \"a\" \"banana\")"),
        "[1,3,5]"
    );
    assert_eq!(
        run("main = Debug.toString (String.any Char.isDigit \"abc1\")"),
        "True"
    );
}
