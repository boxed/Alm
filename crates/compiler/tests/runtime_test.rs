//! Runtime kernel tests: exercise the JavaScript runtime under node —
//! Json, Task, Time, Random, ports, and the long tail of stdlib
//! functions that the DOM-driven suites never touch.

mod common;

/// Pure values: compile `main = <string expr>` and print it.
fn run(body: &str) -> String {
    let source = format!("module Test exposing (..)\n\n{}", body);
    let javascript = common::compile_single("Test.elm", &source);
    let js_path = common::write_js("runtime", &javascript);
    common::run_node(
        &format!(
            "console.log(require({:?})['Test']['main']);",
            js_path.to_str().unwrap()
        ),
        &javascript,
    )
}

/// Effectful programs: a Platform.worker with `ask`/`answer` ports. The
/// harness sends one `ask`, collects `answer` messages for a beat, and
/// prints them.
fn run_worker(body: &str) -> Vec<String> {
    let source = format!(
        "port module Test exposing (main)\n\nimport Json.Decode\n\nport ask : (() -> msg) -> Sub msg\n\nport answer : String -> Cmd msg\n\n{}",
        body
    );
    let javascript = common::compile_single("Test.elm", &source);
    let js_path = common::write_js("runtime-worker", &javascript);
    let script = format!(
        "var Elm = require({:?});\n\
         var app = Elm.Test.main.init({{}});\n\
         var out = [];\n\
         app.ports.answer.subscribe(function (s) {{ out.push(s); }});\n\
         app.ports.ask.send(null);\n\
         setTimeout(function () {{ out.forEach(function (s) {{ console.log(s); }}); process.exit(0); }}, 150);",
        js_path.to_str().unwrap()
    );
    common::run_node(&script, &javascript)
        .lines()
        .map(str::to_string)
        .collect()
}

// JSON — every decoder and encoder

#[test]
fn json_decoders() {
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.nullable Json.Decode.int) "null")"#),
        "Ok Nothing"
    );
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.dict Json.Decode.int) "{\"a\":1,\"b\":2}")"#),
        r#"Ok (Dict.fromList [("a",1),("b",2)])"#
    );
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.keyValuePairs Json.Decode.bool) "{\"x\":true}")"#),
        r#"Ok [("x",True)]"#
    );
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.array Json.Decode.int) "[1,2]")"#),
        "Ok (Array.fromList [1,2])"
    );
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.oneOf [ Json.Decode.map String.fromInt Json.Decode.int, Json.Decode.string ]) "\"hi\"")"#),
        r#"Ok "hi""#
    );
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.null 9) "null")"#),
        "Ok 9"
    );
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.maybe Json.Decode.int) "\"x\"")"#),
        "Ok Nothing"
    );
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.succeed 1) "0")"#),
        "Ok 1"
    );
    // andThen + fail
    assert_eq!(
        run("decoder =\n    Json.Decode.int\n        |> Json.Decode.andThen\n            (\\n ->\n                if n > 0 then\n                    Json.Decode.succeed n\n                else\n                    Json.Decode.fail \"not positive\"\n            )\n\nmain = Debug.toString ( Json.Decode.decodeString decoder \"5\", Result.toMaybe (Json.Decode.decodeString decoder \"-5\") )"),
        "(Ok 5,Nothing)"
    );
}

#[test]
fn json_error_rendering() {
    let field_error = run(r#"main =
    case Json.Decode.decodeString (Json.Decode.field "name" Json.Decode.string) "{\"name\":42}" of
        Ok _ ->
            "no error"

        Err e ->
            Json.Decode.errorToString e"#);
    assert!(field_error.contains(".name"), "got: {}", field_error);
    assert!(field_error.contains("Expecting a STRING"), "got: {}", field_error);

    let index_error = run(r#"main =
    case Json.Decode.decodeString (Json.Decode.index 1 Json.Decode.int) "[1,\"x\"]" of
        Ok _ ->
            "no error"

        Err e ->
            Json.Decode.errorToString e"#);
    assert!(index_error.contains("[1]"), "got: {}", index_error);

    let oneof_error = run(r#"main =
    case Json.Decode.decodeString (Json.Decode.oneOf [ Json.Decode.int, Json.Decode.bool |> Json.Decode.map (\_ -> 0) ]) "\"s\"" of
        Ok _ ->
            "no error"

        Err e ->
            Json.Decode.errorToString e"#);
    assert!(oneof_error.contains("All possibilities failed"), "got: {}", oneof_error);

    let syntax_error = run(r#"main =
    case Json.Decode.decodeString Json.Decode.int "{oops" of
        Ok _ ->
            "no error"

        Err e ->
            Json.Decode.errorToString e"#);
    assert!(syntax_error.contains("not valid JSON"), "got: {}", syntax_error);
}

#[test]
fn json_encoding() {
    assert_eq!(
        run(r#"main =
    Json.Encode.encode 0
        (Json.Encode.object
            [ ( "s", Json.Encode.string "x" )
            , ( "i", Json.Encode.int 1 )
            , ( "f", Json.Encode.float 1.5 )
            , ( "b", Json.Encode.bool True )
            , ( "n", Json.Encode.null )
            , ( "l", Json.Encode.list Json.Encode.int [ 1, 2 ] )
            , ( "a", Json.Encode.array Json.Encode.int (Array.fromList [ 3 ]) )
            , ( "d", Json.Encode.dict identity Json.Encode.int (Dict.singleton "k" 4) )
            , ( "set", Json.Encode.set Json.Encode.int (Set.fromList [ 5 ]) )
            ]
        )"#),
        r#"{"s":"x","i":1,"f":1.5,"b":true,"n":null,"l":[1,2],"a":[3],"d":{"k":4},"set":[5]}"#
    );
    // decodeValue round trip
    assert_eq!(
        run("main = Debug.toString (Json.Decode.decodeValue Json.Decode.int (Json.Encode.int 7))"),
        "Ok 7"
    );
}

// TASKS, SLEEP, TIME, RANDOM — through the worker

#[test]
fn task_machinery() {
    let out = run_worker(
        "type Msg\n    = Ask ()\n    | Chained String\n    | Recovered String\n    | Slept String\n\nupdate msg model =\n    case msg of\n        Ask _ ->\n            ( model\n            , Cmd.batch\n                [ Task.succeed 20\n                    |> Task.map (\\n -> n * 2)\n                    |> Task.andThen (\\n -> Task.map2 (\\a b -> a + b) (Task.succeed n) (Task.succeed 2))\n                    |> Task.perform (\\n -> Chained (String.fromInt n))\n                , Task.fail \"boom\"\n                    |> Task.mapError String.toUpper\n                    |> Task.onError (\\e -> Task.succeed (\"recovered:\" ++ e))\n                    |> Task.perform Recovered\n                , Task.sequence [ Task.succeed 1, Task.succeed 2, Task.succeed 3 ]\n                    |> Task.map (List.map String.fromInt >> String.join \"+\")\n                    |> Task.perform Chained\n                , Process.sleep 10\n                    |> Task.perform (\\_ -> Slept \"awake\")\n                ]\n            )\n\n        Chained s ->\n            ( model, answer (\"chained:\" ++ s) )\n\n        Recovered s ->\n            ( model, answer s )\n\n        Slept s ->\n            ( model, answer (\"slept:\" ++ s) )\n\nmain =\n    Platform.worker\n        { init = \\_ -> ( (), Cmd.none )\n        , update = update\n        , subscriptions = \\_ -> ask Ask\n        }",
    );
    let joined = out.join("|");
    assert!(joined.contains("chained:42"), "got: {}", joined);
    assert!(joined.contains("recovered:BOOM"), "got: {}", joined);
    assert!(joined.contains("chained:1+2+3"), "got: {}", joined);
    assert!(joined.contains("slept:awake"), "got: {}", joined);
}

#[test]
fn task_attempt_and_time() {
    let out = run_worker(
        "type Msg\n    = Ask ()\n    | Got (Result String Time.Posix)\n\nupdate msg model =\n    case msg of\n        Ask _ ->\n            ( model, Task.attempt Got (Task.andThen (\\_ -> Time.now) (Task.succeed ())) )\n\n        Got (Ok posix) ->\n            ( model\n            , answer\n                (if Time.posixToMillis posix > 1000000 then\n                    \"time-ok\"\n                 else\n                    \"time-bad\"\n                )\n            )\n\n        Got (Err e) ->\n            ( model, answer e )\n\nmain =\n    Platform.worker\n        { init = \\_ -> ( (), Cmd.none )\n        , update = update\n        , subscriptions = \\_ -> ask Ask\n        }",
    );
    assert_eq!(out, vec!["time-ok"]);
}

#[test]
fn random_generation() {
    let out = run_worker(
        "type Msg\n    = Ask ()\n    | Got Int\n\nupdate msg model =\n    case msg of\n        Ask _ ->\n            ( model, Random.generate Got (Random.int 1 6) )\n\n        Got n ->\n            ( model\n            , answer\n                (if n >= 1 && n <= 6 then\n                    \"in-range\"\n                 else\n                    \"out-of-range:\" ++ String.fromInt n\n                )\n            )\n\nmain =\n    Platform.worker\n        { init = \\_ -> ( (), Cmd.none )\n        , update = update\n        , subscriptions = \\_ -> ask Ask\n        }",
    );
    assert_eq!(out, vec!["in-range"]);
}

#[test]
fn cmd_map_and_port_conversion() {
    // Cmd.map over a child update, and a typed incoming port payload.
    let source = r#"port module Test exposing (main)

port askList : (List String -> msg) -> Sub msg

port answer : String -> Cmd msg

type Msg
    = GotList (List String)

main : Program () () Msg
main =
    Platform.worker
        { init = \_ -> ( (), Cmd.none )
        , update =
            \(GotList items) model ->
                ( model, Cmd.map identity (answer (String.join "," items)) )
        , subscriptions = \_ -> askList GotList
        }
"#;
    let javascript = common::compile_single("Test.elm", source);
    let js_path = common::write_js("runtime-ports", &javascript);
    let script = format!(
        "var Elm = require({:?});\n\
         var app = Elm.Test.main.init({{}});\n\
         app.ports.answer.subscribe(function (s) {{ console.log(s); process.exit(0); }});\n\
         app.ports.askList.send(['a', 'b', 'c']);",
        js_path.to_str().unwrap()
    );
    assert_eq!(common::run_node(&script, &javascript), "a,b,c");
}

// TIME — pure conversions

#[test]
fn time_conversions() {
    assert_eq!(
        run("t = Time.millisToPosix 1720000000000\n\nmain =\n    String.join \" \"\n        [ String.fromInt (Time.toYear Time.utc t)\n        , Debug.toString (Time.toMonth Time.utc t)\n        , String.fromInt (Time.toDay Time.utc t)\n        , String.fromInt (Time.toHour Time.utc t)\n        , String.fromInt (Time.toMinute Time.utc t)\n        , String.fromInt (Time.toSecond Time.utc t)\n        , String.fromInt (Time.toMillis Time.utc t)\n        , Debug.toString (Time.toWeekday Time.utc t)\n        , String.fromInt (Time.posixToMillis t)\n        ]"),
        "2024 Jul 3 9 46 40 0 Wed 1720000000000"
    );
    // custom zones shift the clock
    assert_eq!(
        run("zone = Time.customZone 60 []\n\nmain = String.fromInt (Time.toHour zone (Time.millisToPosix 0))"),
        "1"
    );
}

// RANDOM — pure stepping is deterministic

#[test]
fn random_step_deterministic() {
    assert_eq!(
        run("gen = Random.map2 (\\a b -> ( a, b )) (Random.int 0 100) (Random.constant 5)\n\nstep seed = Random.step gen seed\n\nmain =\n    let\n        ( ( a1, b1 ), _ ) =\n            step (Random.initialSeed 42)\n\n        ( ( a2, b2 ), _ ) =\n            step (Random.initialSeed 42)\n\n        ( xs, _ ) =\n            Random.step (Random.list 3 (Random.andThen Random.constant (Random.float 0 1))) (Random.initialSeed 7)\n    in\n    Debug.toString ( a1 == a2 && b1 == 5, List.length xs, List.all (\\x -> x >= 0 && x <= 1) xs )"),
        "(True,3,True)"
    );
}

// URL / UUID / HTTP helpers — pure

#[test]
fn url_parsing() {
    assert_eq!(
        run(r#"main =
    case Url.fromString "https://example.com:8080/a/b?q=1#frag" of
        Just url ->
            Url.toString url ++ "|" ++ Debug.toString url.protocol ++ "|" ++ Debug.toString url.port_

        Nothing ->
            "parse failed""#),
        "https://example.com:8080/a/b?q=1#frag|Https|Just 8080"
    );
    assert_eq!(run(r#"main = Debug.toString (Url.fromString "not a url")"#), "Nothing");
    assert_eq!(
        run(r#"main = Debug.toString ( Url.percentEncode "a b/c", Url.percentDecode "a%20b" )"#),
        r#"("a%20b%2Fc",Just "a b")"#
    );
}

#[test]
fn uuid_helpers() {
    assert_eq!(
        run(r#"main =
    case UUID.fromString "123e4567-E89B-12d3-a456-426614174000" of
        Ok uuid ->
            UUID.toString uuid ++ "|" ++ UUID.toRepresentation UUID.Compact uuid

        Err _ ->
            "bad uuid""#),
        "123e4567-e89b-12d3-a456-426614174000|123e4567e89b12d3a456426614174000"
    );
    assert_eq!(
        run(r#"main = Debug.toString (Result.toMaybe (UUID.fromString "nope"))"#),
        "Nothing"
    );
}

#[test]
fn http_progress_math() {
    assert_eq!(
        run("main = Debug.toString ( Http.fractionSent { sent = 25, size = 100 }, Http.fractionReceived { received = 5, size = Just 10 }, Http.fractionReceived { received = 5, size = Nothing } )"),
        "(0.25,0.5,0)"
    );
}

// STDLIB LONG TAIL

#[test]
fn list_long_tail() {
    assert_eq!(
        run("main = String.join \"|\" [ Debug.toString (List.singleton 1), Debug.toString (List.head []), Debug.toString (List.tail [ 1, 2 ]), Debug.toString (List.minimum [ 3, 1 ]), Debug.toString (List.product [ 2, 3, 4 ]), Debug.toString (List.concat [ [ 1 ], [ 2, 3 ] ]) ]"),
        "[1]|Nothing|Just [2]|Just 1|24|[1,2,3]"
    );
    assert_eq!(
        run("main = Debug.toString ( List.length [ 1, 2, 3 ], List.reverse [ 1, 2 ], List.intersperse 0 [ 1, 2, 3 ] )"),
        "(3,[2,1],[1,0,2,0,3])"
    );
}

#[test]
fn string_long_tail() {
    assert_eq!(
        run(r#"main = String.join "|" [ Debug.toString (String.isEmpty ""), Debug.toString (String.words "  a b  c "), Debug.toString (String.lines "x\ny"), Debug.toString (String.toFloat "1.5"), Debug.toString (String.toFloat "zzz") ]"#),
        r#"True|["a","b","c"]|["x","y"]|Just 1.5|Nothing"#
    );
    assert_eq!(
        run(r#"main = String.join "|" [ String.fromList [ 'a', 'b' ], String.toLower "ABC", String.trim " x ", String.trimLeft " y", String.trimRight "z " ]"#),
        "ab|abc|x|y|z"
    );
}

#[test]
fn char_and_basics_long_tail() {
    assert_eq!(
        run("main = String.join \"|\" [ Debug.toString (Char.fromCode 66), Debug.toString (Char.isAlpha 'a'), Debug.toString (Char.isUpper 'a'), Debug.toString (Char.isLower 'a'), Debug.toString (Char.toUpper 'a'), Debug.toString (Char.toLower 'A'), Debug.toString (Char.isAlphaNum '_'), Debug.toString (Char.isHexDigit 'f') ]"),
        r#""B"|True|False|True|"A"|"a"|False|True"#
    );
    assert_eq!(
        run("main = String.join \"|\" [ Debug.toString (truncate -3.7), Debug.toString (abs -4), Debug.toString (identity 9), Debug.toString (isNaN (0 / 0)), Debug.toString (isInfinite (1 / 0)) ]"),
        "-3|4|9|True|True"
    );
    assert_eq!(
        run("main = Debug.toString ( degrees 180 == pi, turns 0.5 == pi, radians pi == pi )"),
        "(True,True,True)"
    );
    assert_eq!(
        run("main =\n    let\n        ( r, theta ) =\n            toPolar ( 3, 4 )\n\n        ( x, y ) =\n            fromPolar ( 5, 0 )\n    in\n    Debug.toString ( r, round x, round y )"),
        "(5,5,0)"
    );
}

#[test]
fn collections_long_tail() {
    assert_eq!(
        run("d = Dict.fromList [ ( 1, \"a\" ), ( 2, \"b\" ) ]\n\nmain = Debug.toString ( Dict.isEmpty d, Dict.size d, Dict.values d )"),
        "(False,2,[\"a\",\"b\"])"
    );
    assert_eq!(
        run("main = Debug.toString ( Set.isEmpty Set.empty, Set.size (Set.singleton 3), Set.toList (Set.singleton 3) )"),
        "(True,1,[3])"
    );
    assert_eq!(
        run("arr = Array.fromList [ 10, 20 ]\n\nmain = Debug.toString ( Array.isEmpty arr, Array.length arr, Array.toIndexedList arr )"),
        "(False,2,[(0,10),(1,20)])"
    );
    assert_eq!(
        run("main = String.join \"|\" [ Debug.toString (Bitwise.complement 0), Debug.toString (Tuple.mapFirst negate ( 1, 2 )), Debug.toString (Tuple.mapSecond negate ( 1, 2 )), Debug.toString (Tuple.mapBoth negate String.fromInt ( 1, 2 )) ]"),
        "-1|(-1,2)|(1,-2)|(-1,\"2\")"
    );
    assert_eq!(
        run("main = Debug.toString ( Maybe.map3 (\\a b c -> a + b + c) (Just 1) (Just 2) (Just 3), Maybe.map4 (\\a b c d -> a + b + c + d) (Just 1) (Just 2) (Just 3) Nothing, Result.fromMaybe \"e\" (Just 1) )"),
        "(Just 6,Nothing,Ok 1)"
    );
}
