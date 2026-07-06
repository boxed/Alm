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
    // Matches elm's wording: "The Json.Decode.oneOf … failed in the following N ways:".
    assert!(oneof_error.contains("failed in the following 2 ways"), "got: {}", oneof_error);

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

// URL / HTTP helpers — pure

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
        // Chars render with single quotes, matching elm's dev-build Debug.toString.
        r#"'B'|True|False|True|'A'|'a'|False|True"#
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

// PORT PAYLOAD CONVERSION — every converter shape, both directions

#[test]
fn port_payload_shapes() {
    let source = r#"port module Test exposing (main)

port askRecord : ({ n : Int, tags : List String } -> msg) -> Sub msg

port askTriple : (( Int, String, Bool ) -> msg) -> Sub msg

port askMaybe : (Maybe Int -> msg) -> Sub msg

port askArray : (Array.Array Float -> msg) -> Sub msg

port sendRecord : { n : Int, ok : Bool } -> Cmd msg

port sendPair : ( Int, Maybe String ) -> Cmd msg

port sendUnit : () -> Cmd msg

port sendList : List Int -> Cmd msg

import Array

type Msg
    = GotRecord { n : Int, tags : List String }
    | GotTriple ( Int, String, Bool )
    | GotMaybe (Maybe Int)
    | GotArray (Array.Array Float)

update : Msg -> () -> ( (), Cmd Msg )
update msg model =
    case msg of
        GotRecord r ->
            ( model
            , Cmd.batch
                [ sendRecord { n = r.n * 2, ok = not (List.isEmpty r.tags) }
                , sendList (List.map String.length r.tags)
                ]
            )

        GotTriple ( n, s, b ) ->
            ( model, sendPair ( n + String.length s, if b then Just s else Nothing ) )

        GotMaybe m ->
            ( model
            , case m of
                Just n ->
                    sendPair ( n, Nothing )

                Nothing ->
                    sendUnit ()
            )

        GotArray arr ->
            ( model, sendList (List.map round (Array.toList arr)) )

main : Program () () Msg
main =
    Platform.worker
        { init = \_ -> ( (), Cmd.none )
        , update = update
        , subscriptions =
            \_ ->
                Sub.batch
                    [ askRecord GotRecord
                    , askTriple GotTriple
                    , askMaybe GotMaybe
                    , askArray GotArray
                    ]
        }
"#;
    // imports must precede ports: rebuild source in legal order
    let source = source.replace(
        "port sendList : List Int -> Cmd msg\n\nimport Array\n",
        "port sendList : List Int -> Cmd msg\n",
    );
    let source = source.replace(
        "port module Test exposing (main)\n",
        "port module Test exposing (main)\n\nimport Array\n",
    );
    let javascript = common::compile_single("Test.elm", &source);
    let js_path = common::write_js("runtime-payloads", &javascript);
    let script = format!(
        r#"var Elm = require({:?});
var app = Elm.Test.main.init({{}});
var log = [];
app.ports.sendRecord.subscribe(function (v) {{ log.push('record:' + JSON.stringify(v)); }});
app.ports.sendPair.subscribe(function (v) {{ log.push('pair:' + JSON.stringify(v)); }});
app.ports.sendUnit.subscribe(function (v) {{ log.push('unit:' + JSON.stringify(v)); }});
app.ports.sendList.subscribe(function (v) {{ log.push('list:' + JSON.stringify(v)); }});
app.ports.askRecord.send({{ n: 21, tags: ['ab', 'cde'] }});
app.ports.askTriple.send([4, 'xyz', true]);
app.ports.askMaybe.send(7);
app.ports.askMaybe.send(null);
app.ports.askArray.send([1.4, 2.6]);
setTimeout(function () {{ log.forEach(function (s) {{ console.log(s); }}); process.exit(0); }}, 100);"#,
        js_path.to_str().unwrap()
    );
    let out = common::run_node(&script, &javascript);
    assert!(out.contains(r#"record:{"n":42,"ok":true}"#), "got: {}", out);
    assert!(out.contains("list:[2,3]"), "got: {}", out);
    assert!(out.contains(r#"pair:[7,"xyz"]"#), "got: {}", out);
    assert!(out.contains("pair:[7,null]"), "got: {}", out);
    assert!(out.contains("unit:null"), "got: {}", out);
    assert!(out.contains("list:[1,3]"), "got: {}", out);
}

#[test]
fn outgoing_array_and_value_ports() {
    let source = r#"port module Test exposing (main)

import Array
import Json.Encode

port askUnit : (() -> msg) -> Sub msg

port sendArray : Array.Array Int -> Cmd msg

port sendValue : Json.Encode.Value -> Cmd msg

type Msg
    = Go ()

main : Program () () Msg
main =
    Platform.worker
        { init = \_ -> ( (), Cmd.none )
        , update =
            \(Go _) model ->
                ( model
                , Cmd.batch
                    [ sendArray (Array.fromList [ 7, 8 ])
                    , sendValue (Json.Encode.object [ ( "raw", Json.Encode.bool True ) ])
                    ]
                )
        , subscriptions = \_ -> askUnit Go
        }
"#;
    let javascript = common::compile_single("Test.elm", source);
    let js_path = common::write_js("runtime-outarray", &javascript);
    let script = format!(
        r#"var Elm = require({:?});
var app = Elm.Test.main.init({{}});
var log = [];
app.ports.sendArray.subscribe(function (v) {{ log.push('arr:' + JSON.stringify(v)); }});
app.ports.sendValue.subscribe(function (v) {{ log.push('val:' + JSON.stringify(v)); }});
app.ports.askUnit.send(null);
setTimeout(function () {{ console.log(log.join('|')); process.exit(0); }}, 80);"#,
        js_path.to_str().unwrap()
    );
    let out = common::run_node(&script, &javascript);
    assert!(out.contains("arr:[7,8]"), "got: {}", out);
    assert!(out.contains(r#"val:{"raw":true}"#), "got: {}", out);
}

// MUTATION KILLS — pin behavior that single-operator mutants can break.

#[test]
fn operator_sections_hit_the_kernel_functions() {
    // Inline codegen handles infix uses; sections go through $Basics$*.
    assert_eq!(
        run("main = Debug.toString ( List.foldl (-) 0 [ 1, 2 ], List.foldl (*) 1 [ 3, 4 ], List.map2 (//) [ 7, 9 ] [ 2, 4 ] )"),
        "(1,12,[3,2])"
    );
    assert_eq!(
        run("main = Debug.toString ( List.map2 (/=) [ 1, 2 ] [ 1, 3 ], List.map2 (==) [ 1 ] [ 1 ] )"),
        "([False,True],[True])"
    );
    // Comparison sections on EQUAL values pin strictness.
    assert_eq!(
        run("main = Debug.toString ( List.map2 (<) [ 1, 1 ] [ 1, 2 ], List.map2 (<=) [ 1, 2 ] [ 1, 1 ], List.map2 (>) [ 1, 2 ] [ 1, 1 ] )"),
        "([False,True],[True,False],[False,True])"
    );
    assert_eq!(
        run("main = Debug.toString ( List.map2 (>=) [ 1, 1 ] [ 1, 2 ], List.map2 (&&) [ True, True ] [ True, False ], List.map2 (||) [ False, False ] [ True, False ] )"),
        "([True,False],[True,False],[True,False])"
    );
    assert_eq!(
        run("main = String.join \"|\" [ Debug.toString (List.map2 xor [ True, True ] [ True, False ]), String.append \"a\" \"b\", Debug.toString (List.map2 max [ 1, 5 ] [ 2, 2 ]), Debug.toString (List.map2 min [ 1, 5 ] [ 2, 2 ]) ]"),
        "[False,True]|ab|[2,5]|[1,2]"
    );
}

#[test]
fn compare_equal_values() {
    assert_eq!(
        run("main = Debug.toString [ compare 1 1, compare ( 1, \"a\", 'c' ) ( 1, \"a\", 'c' ), compare [ 1 ] [ 1 ], compare [ 1 ] [ 1, 2 ], compare [ 1, 2 ] [ 1 ], compare \"a\" \"a\" ]"),
        "[EQ,EQ,EQ,LT,GT,EQ]"
    );
}

#[test]
fn negative_modulus_and_remainder() {
    assert_eq!(
        run("main = Debug.toString [ modBy -3 6, modBy -3 7, modBy 3 7, modBy 1 5, remainderBy 3 7, remainderBy 3 -7 ]"),
        "[0,-2,1,0,1,-1]"
    );
    assert_eq!(
        run("main = Debug.toString [ abs 0.5, abs -0.5, clamp 1 5 0.5, clamp 1 5 9, clamp 1 5 3 ]"),
        "[0.5,0.5,1,5,3]"
    );
    assert_eq!(
        run("main = Debug.toString ( truncate 2.9, truncate -3.7 )"),
        "(2,-3)"
    );
}

#[test]
fn string_slice_boundaries() {
    assert_eq!(
        run(r#"main = Debug.toString [ String.left 0 "abc", String.left 2 "abc", String.right 0 "abc", String.right 2 "abc" ]"#),
        r#"["","ab","","bc"]"#
    );
    assert_eq!(
        run(r#"main = Debug.toString [ String.dropLeft 0 "abc", String.dropLeft 2 "abc", String.dropRight 0 "abc", String.dropRight 2 "abc" ]"#),
        r#"["abc","c","abc","a"]"#
    );
    assert_eq!(
        run(r#"main = Debug.toString [ String.repeat 0 "x", String.cons 'a' "bc" ]"#),
        r#"["","abc"]"#
    );
    assert_eq!(
        run(r#"main = Debug.toString [ String.contains "zz" "az", String.contains "az" "az", String.startsWith "b" "ab" ]"#),
        "[False,True,False]"
    );
}

#[test]
fn char_predicate_boundaries() {
    // Characters adjacent to the class boundaries kill range mutants.
    assert_eq!(
        run("main = Debug.toString (List.map Char.isDigit [ '/', '0', '9', ':' ])"),
        "[False,True,True,False]"
    );
    assert_eq!(
        run("main = Debug.toString (List.map Char.isUpper [ '@', 'A', 'Z', '[', 'a' ])"),
        "[False,True,True,False,False]"
    );
    assert_eq!(
        run("main = Debug.toString (List.map Char.isLower [ '`', 'a', 'z', '{', 'A' ])"),
        "[False,True,True,False,False]"
    );
    assert_eq!(
        run("main = Debug.toString (List.map Char.isAlpha [ 'B', 'y', '+', '5', '[' ])"),
        "[True,True,False,False,False]"
    );
    assert_eq!(
        run("main = Debug.toString (List.map Char.isAlphaNum [ 'B', 'y', '8', '9', '+', '[' ])"),
        "[True,True,True,True,False,False]"
    );
    assert_eq!(
        run("main = Debug.toString (List.map Char.isHexDigit [ '0', '9', 'a', 'f', 'g', 'A', 'F', 'G' ])"),
        "[True,True,True,True,False,True,True,False]"
    );
}

#[test]
fn maybe_short_circuits_every_position() {
    assert_eq!(
        run("f = Maybe.map3 (\\a b c -> a + b + c)\n\nmain = Debug.toString [ f Nothing (Just 2) (Just 3), f (Just 1) Nothing (Just 3), f (Just 1) (Just 2) Nothing, f (Just 1) (Just 2) (Just 3) ]"),
        "[Nothing,Nothing,Nothing,Just 6]"
    );
    assert_eq!(
        run("f = Maybe.map4 (\\a b c d -> a + b + c + d)\n\nmain = Debug.toString [ f Nothing (Just 2) (Just 3) (Just 4), f (Just 1) Nothing (Just 3) (Just 4), f (Just 1) (Just 2) Nothing (Just 4), f (Just 1) (Just 2) (Just 3) (Just 4) ]"),
        "[Nothing,Nothing,Nothing,Just 10]"
    );
}

#[test]
fn dict_binary_search_boundaries() {
    // Multi-key dicts probe first/last/missing on both sides plus
    // overwrite, exercising every branch of the binary search.
    assert_eq!(
        run("d = Dict.fromList [ ( 10, \"a\" ), ( 20, \"b\" ), ( 30, \"c\" ), ( 40, \"d\" ), ( 50, \"e\" ) ]\n\nmain = Debug.toString [ Dict.get 10 d, Dict.get 50 d, Dict.get 5 d, Dict.get 55 d, Dict.get 25 d, Dict.get 30 d ]"),
        "[Just \"a\",Just \"e\",Nothing,Nothing,Nothing,Just \"c\"]"
    );
    assert_eq!(
        run("d = Dict.fromList [ ( 2, \"x\" ), ( 1, \"y\" ) ]\n\nmain = String.join \"|\" [ Debug.toString (Dict.toList (Dict.insert 1 \"z\" d)), Debug.toString ( Dict.member 1 d, Dict.member 3 d ), Debug.toString (Dict.toList (Dict.remove 9 d)) ]"),
        "[(1,\"z\"),(2,\"x\")]|(True,False)|[(1,\"y\"),(2,\"x\")]"
    );
    assert_eq!(
        run("main = String.join \"|\" [ Debug.toString ( Array.get -1 (Array.fromList [ 1 ]), Array.get 1 (Array.fromList [ 1 ]) ), Debug.toString (Array.isEmpty Array.empty), Debug.toString (Array.toList (Array.set 5 9 (Array.fromList [ 1 ]))) ]"),
        "(Nothing,Nothing)|True|[1]"
    );
}

#[test]
fn json_decoder_edges() {
    assert_eq!(
        run(r#"main = Debug.toString ( Json.Decode.decodeString Json.Decode.int "1.5" |> Result.toMaybe, Json.Decode.decodeString (Json.Decode.nullable Json.Decode.int) "3" )"#),
        "(Nothing,Ok (Just 3))"
    );
    assert_eq!(
        run(r#"main = Debug.toString ( Result.toMaybe (Json.Decode.decodeString (Json.Decode.field "k" Json.Decode.int) "9"), Result.toMaybe (Json.Decode.decodeString (Json.Decode.index 2 Json.Decode.int) "[1,2]"), Json.Decode.decodeString (Json.Decode.index 1 Json.Decode.int) "[1,2]" )"#),
        "(Nothing,Nothing,Ok 2)"
    );
    assert_eq!(
        run(r#"main = Debug.toString ( Result.toMaybe (Json.Decode.decodeString (Json.Decode.keyValuePairs Json.Decode.int) "[1]"), Result.toMaybe (Json.Decode.decodeString (Json.Decode.dict Json.Decode.int) "null") )"#),
        "(Nothing,Nothing)"
    );
    // Comparing kernel null values goes through _Utils_eq's null guard.
    assert_eq!(
        run(r#"main = Debug.toString ( Json.Encode.null == Json.Encode.string "x", Json.Encode.null == Json.Encode.null )"#),
        "(False,True)"
    );
}

#[test]
fn debug_tostring_parenthesization() {
    assert_eq!(
        run(r#"type Tag
    = Tag String { n : Int } (List Int) ( Int, Int )

main = Debug.toString (Tag "a b" { n = 1 } [ 1 ] ( 1, 2 ))"#),
        r#"Tag "a b" { n = 1 } [1] (1,2)"#
    );
    assert_eq!(
        run("main = Debug.toString (Just (Just 1))"),
        "Just (Just 1)"
    );
}

/// Exact PRNG outputs for the current mulberry32 implementation.
const PINNED_RANDOM: &str = "(916341,132144,950847)";

#[test]
fn random_pinned_outputs() {
    // Pin the PRNG algorithm exactly: any constant or operator mutation
    // inside _Random_next changes these values.
    let program = "main =\n    let\n        ( a, s2 ) =\n            Random.step (Random.int 0 999999) (Random.initialSeed 42)\n\n        ( b, _ ) =\n            Random.step (Random.int 0 999999) s2\n\n        ( c, _ ) =\n            Random.step (Random.float 0 1) (Random.initialSeed 7)\n    in\n    Debug.toString ( a, b, truncate (c * 1000000) )";
    let out = run(program);
    assert!(out.starts_with('(') && out.split(',').count() == 3, "got: {}", out);
    // Pin the exact values so any PRNG constant/operator mutation is caught.
    assert_eq!(out, PINNED_RANDOM, "PRNG output changed — update PINNED_RANDOM if intentional");
}

#[test]
fn random_bounds_and_constants() {
    assert_eq!(
        run("main = Debug.toString ( Random.minInt, Random.maxInt )"),
        "(-2147483648,2147483647)"
    );
    // Tight bounds make off-by-one in the range formula visible.
    assert_eq!(
        run("main =\n    let\n        ( v, _ ) =\n            Random.step (Random.int 5 5) (Random.initialSeed 1)\n\n        ( w, _ ) =\n            Random.step (Random.int 0 1) (Random.initialSeed 2)\n    in\n    Debug.toString ( v, w >= 0 && w <= 1 )"),
        "(5,True)"
    );
}

#[test]
fn url_field_permutations() {
    assert_eq!(
        run(r#"show s =
    case Url.fromString s of
        Just u ->
            Debug.toString ( u.port_, u.query, u.fragment )

        Nothing ->
            "no parse"

main =
    String.join " | "
        [ show "http://x.com/p"
        , show "http://x.com:80/p?q=2"
        , show "http://x.com/p#f"
        , show "http://x.com/?#"
        ]"#),
        r#"(Nothing,Nothing,Nothing) | (Just 80,Just "q=2",Nothing) | (Nothing,Nothing,Just "f") | (Nothing,Just "",Just "")"#
    );
}

#[test]
fn time_zone_eras() {
    // A zone with eras: times after the era start use the era offset,
    // older times fall back to the default.
    assert_eq!(
        run("zone = Time.customZone 0 [ { start = 100, offset = 120 } ]\n\nmain = Debug.toString ( Time.toHour zone (Time.millisToPosix 12000000), Time.toHour zone (Time.millisToPosix 0) )"),
        "(5,0)"
    );
}

#[test]
fn http_progress_boundaries() {
    assert_eq!(
        run("main = Debug.toString ( Http.fractionSent { sent = 0, size = 0 }, Http.fractionReceived { received = 1, size = Just 0 } )"),
        "(1,0)"
    );
}

#[test]
fn string_indexes_and_number_parsing() {
    assert_eq!(
        run(r#"main = Debug.toString ( String.indexes "aa" "aaaa", String.toFloat "", String.toFloat "5e1x" )"#),
        "([0,2],Nothing,Nothing)"
    );
    assert_eq!(
        run("main = String.join \"|\" [ Debug.toString (List.maximum [ 5, 5, 3 ]), Debug.toString (List.minimum [ 3, 3, 5 ]), Debug.toString ( List.all (\\x -> x > 0) [ 1, -1 ], List.all (\\x -> x > 0) [ 1, 2 ] ) ]"),
        "Just 5|Just 3|(False,True)"
    );
}

// MUTATION KILLS, wave 3

#[test]
fn equality_ignores_no_extra_keys() {
    assert_eq!(
        run(r#"main = Debug.toString ( Json.Encode.object [ ( "a", Json.Encode.int 1 ) ] == Json.Encode.object [ ( "a", Json.Encode.int 1 ), ( "b", Json.Encode.int 2 ) ] )"#),
        "False"
    );
}

#[test]
fn compare_pairs_and_triples() {
    assert_eq!(
        run("main = Debug.toString [ compare ( 1, 2 ) ( 1, 2 ), compare ( 1, 1, 'a' ) ( 1, 1, 'b' ), compare ( 1, 1, 'b' ) ( 1, 1, 'a' ) ]"),
        "[EQ,LT,GT]"
    );
}

#[test]
fn string_slices_at_one() {
    assert_eq!(
        run(r#"main = Debug.toString [ String.left 1 "abc", String.right 1 "abc", String.dropLeft 1 "abc", String.dropRight 1 "abc", String.repeat 1 "ab" ]"#),
        r#"["a","c","bc","ab","ab"]"#
    );
}

#[test]
fn infinity_and_polar_angles() {
    assert_eq!(
        run("main = Debug.toString [ isInfinite (-1 / 0), isInfinite 1, isInfinite (1 / 0) ]"),
        "[True,False,True]"
    );
    // A non-trivial angle distinguishes multiply from divide.
    assert_eq!(
        run("main =\n    let\n        ( x, y ) =\n            fromPolar ( 2, pi / 2 )\n    in\n    Debug.toString ( round (x * 1000), round y )"),
        "(0,2)"
    );
}

#[test]
fn hex_digit_interior_chars() {
    assert_eq!(
        run("main = Debug.toString (List.map Char.isHexDigit [ '5', 'c', 'C', '+' ])"),
        "[True,True,True,False]"
    );
}

#[test]
fn dict_model_fuzz() {
    // Drive Dict through a fixed op sequence and compare against the
    // same operations replayed on an association list.
    assert_eq!(
        run("step op ( dict, model ) =\n    case op of\n        ( 0, k ) ->\n            ( Dict.insert k (k * 2) dict\n            , ( k, k * 2 ) :: List.filter (\\( mk, _ ) -> mk /= k) model\n            )\n\n        ( _, k ) ->\n            ( Dict.remove k dict\n            , List.filter (\\( mk, _ ) -> mk /= k) model\n            )\n\nops =\n    [ ( 0, 5 ), ( 0, 3 ), ( 0, 9 ), ( 0, 1 ), ( 0, 7 ), ( 1, 3 ), ( 0, 4 ), ( 1, 9 ), ( 0, 5 ), ( 1, 2 ), ( 0, 8 ), ( 0, 2 ), ( 1, 1 ) ]\n\nmain =\n    let\n        ( dict, model ) =\n            List.foldl step ( Dict.empty, [] ) ops\n\n        sortedModel =\n            List.sortBy Tuple.first model\n    in\n    Debug.toString ( Dict.toList dict == sortedModel, Dict.toList dict, List.map (\\k -> Dict.get k dict) (List.range 0 10) )"),
        "(True,[(2,4),(4,8),(5,10),(7,14),(8,16)],[Nothing,Nothing,Just 4,Nothing,Just 8,Just 10,Nothing,Just 14,Just 16,Nothing,Nothing])"
    );
}

#[test]
fn array_index_boundaries() {
    assert_eq!(
        run("arr = Array.fromList [ 10, 20 ]\n\nmain = Debug.toString [ Array.get 0 arr, Array.get 1 arr, Array.get 2 arr ]"),
        "[Just 10,Just 20,Nothing]"
    );
    assert_eq!(
        run("main = Debug.toString ( Array.toList (Array.set 1 9 (Array.fromList [ 5 ])), Array.toList (Array.set 0 9 (Array.fromList [ 5 ])) )"),
        "([5],[9])"
    );
}

#[test]
fn bitwise_or_and_xor_values() {
    assert_eq!(
        run("main = Debug.toString [ Bitwise.or 12 10, Bitwise.xor 12 10, Bitwise.and 12 10 ]"),
        "[14,6,8]"
    );
}

#[test]
fn json_error_messages_distinguish_failures() {
    assert_eq!(
        run(r#"describe r =
    case r of
        Ok _ ->
            "ok"

        Err e ->
            Json.Decode.errorToString e

main =
    String.join " %% "
        [ describe (Json.Decode.decodeString (Json.Decode.field "k" Json.Decode.int) "{}")
        , describe (Json.Decode.decodeString (Json.Decode.index 2 Json.Decode.int) "[1,2]")
        ]"#)
        .contains("field named `k`")
        .then_some("both")
        .is_some()
        && run(r#"main =
    case Json.Decode.decodeString (Json.Decode.index 2 Json.Decode.int) "[1,2]" of
        Ok _ ->
            "ok"

        Err e ->
            Json.Decode.errorToString e"#)
            .contains("LONGER"),
        true
    );
    assert_eq!(
        run(r#"main = Debug.toString (Json.Decode.decodeString (Json.Decode.oneOf [ Json.Decode.int, Json.Decode.fail "no" ]) "3")"#),
        "Ok 3"
    );
}

#[test]
fn time_era_boundary_and_millis() {
    // Era boundaries are minute-granular: elm compares `flooredDiv ms 60000`
    // (whole minutes) against era.start, so 12000000ms and 12000001ms both fall
    // in minute 200. era.start=200 is NOT `< 200`, so the era does not apply and
    // both use the default offset 0 -> hour 3. (Matches elm/time exactly.)
    assert_eq!(
        run("zone = Time.customZone 0 [ { start = 200, offset = 120 } ]\n\nmain = Debug.toString [ Time.toHour zone (Time.millisToPosix 12000000), Time.toHour zone (Time.millisToPosix 12000001), Time.toMillis zone (Time.millisToPosix 12000001) ]"),
        "[3,3,1]"
    );
}

#[test]
fn http_fraction_small_sizes() {
    assert_eq!(
        run("main = Debug.toString ( Http.fractionSent { sent = 0, size = 1 }, Http.fractionReceived { received = 1, size = Just 1 } )"),
        "(0,1)"
    );
}

#[test]
fn random_full_precision_and_combinators() {
    let program = "main =\n    let\n        ( c, _ ) =\n            Random.step (Random.float 0 1) (Random.initialSeed 7)\n\n        ( pair, _ ) =\n            Random.step (Random.map2 Tuple.pair (Random.int 0 999999) (Random.int 0 999999)) (Random.initialSeed 11)\n\n        ( xs, _ ) =\n            Random.step (Random.list 3 (Random.int 0 99)) (Random.initialSeed 13)\n\n        ( nested, _ ) =\n            Random.step (Random.andThen (\\n -> Random.int n (n + 1)) (Random.int 10 20)) (Random.initialSeed 17)\n    in\n    Debug.toString ( c, pair, ( xs, nested ) )";
    let out = run(program);
    assert_eq!(out, PINNED_RANDOM_FULL, "PRNG chain output changed");
}

/// Full-precision pinned PRNG outputs (see random_pinned_outputs).
const PINNED_RANDOM_FULL: &str = "(0.9508476133806495,(134972,563683),([23,89,86],13))";

#[test]
fn animation_frame_subscription_under_node() {
    let out = run_worker(
        "type Msg\n    = Ask ()\n    | Tick Float\n\nupdate msg ( model, done ) =\n    case msg of\n        Ask _ ->\n            ( ( True, done ), Cmd.none )\n\n        Tick delta ->\n            if done then\n                ( ( model, done ), Cmd.none )\n            else\n                ( ( model, True ), answer (\"tick:\" ++ Debug.toString (delta >= 0)) )\n\nsubscriptions ( active, _ ) =\n    Sub.batch\n        [ ask Ask\n        , if active then\n            Browser.Events.onAnimationFrameDelta Tick\n          else\n            Sub.none\n        ]\n\nmain =\n    Platform.worker\n        { init = \\_ -> ( ( False, False ), Cmd.none )\n        , update = update\n        , subscriptions = subscriptions\n        }",
    );
    assert_eq!(out, vec!["tick:True"]);
}

// MUTATION KILLS, wave 4

#[test]
fn tostring_parenthesizes_space_containing_delimited_args() {
    assert_eq!(
        run(r#"type Wrap
    = Wrap ( Int, String ) (List String)

main = Debug.toString (Wrap ( 1, "a b" ) [ "c d" ])"#),
        r#"Wrap (1,"a b") ["c d"]"#
    );
    assert_eq!(run("main = Debug.toString Json.Encode.null"), "<internal>");
}

#[test]
fn dict_and_set_folds_pin_keys() {
    assert_eq!(
        run("d = Dict.fromList [ ( 1, \"a\" ), ( 2, \"b\" ) ]\n\nmain = Dict.foldl (\\k v acc -> acc ++ String.fromInt k ++ v) \"\" d ++ \"/\" ++ Dict.foldr (\\k v acc -> acc ++ String.fromInt k ++ v) \"\" d"),
        "1a2b/2b1a"
    );
    assert_eq!(
        run("s = Set.fromList [ 3, 1 ]\n\nmain = Set.foldl (\\k acc -> acc ++ String.fromInt k) \"\" s"),
        "13"
    );
    assert_eq!(
        run("main = Debug.toString ( Dict.map (\\k v -> k + v) (Dict.fromList [ ( 1, 10 ), ( 2, 20 ) ]) |> Dict.values, Dict.filter (\\k _ -> k > 1) (Dict.fromList [ ( 1, 10 ), ( 2, 20 ) ]) |> Dict.keys )"),
        "([11,22],[2])"
    );
}

#[test]
fn random_float_ranges_and_seed_threading() {
    let program = "main =\n    let\n        ( f1, s1 ) =\n            Random.step (Random.float 2 5) (Random.initialSeed 3)\n\n        ( p, s2 ) =\n            Random.step (Random.map2 Tuple.pair (Random.int 0 9) (Random.int 0 9)) s1\n\n        ( after, _ ) =\n            Random.step (Random.int 0 999999) s2\n\n        ( nested, _ ) =\n            Random.step (Random.andThen (\\n -> Random.int 0 (n * 100000)) (Random.int 1 9)) (Random.initialSeed 23)\n    in\n    Debug.toString ( f1, p, ( after, nested ) )";
    assert_eq!(run(program), PINNED_RANDOM_THREADED);
}

const PINNED_RANDOM_THREADED: &str = "(3.897207004541149,(2,4),(60852,26829))";

// A legal cycle among top-level *values* where one member is initialized
// eagerly (it destructures a sibling parser at construction, like
// `ParserFast.map3`) while the back-reference is deferred behind a lambda.
// Such values must be emitted as lazy thunks and forced in a cycle, exactly
// as Elm does; naive in-order emission reads the sibling before it exists and
// throws `Cannot read properties of undefined`. This underpins
// `stil4m/elm-syntax`, which broke before the thunk-based cycle codegen.
#[test]
fn cyclic_top_level_values_use_lazy_thunks() {
    let program = "type Parser = Parser (Int -> Int)\n\
\n\
run : Parser -> Int -> Int\n\
run (Parser f) i = f i\n\
\n\
seq : Parser -> Parser -> Parser\n\
seq (Parser f) (Parser g) = Parser (\\i -> if i < 0 then f i else g i)\n\
\n\
lazy : (() -> Parser) -> Parser\n\
lazy thunk = Parser (\\i -> run (thunk ()) i)\n\
\n\
top : Parser\n\
top = seq inner leaf\n\
\n\
inner : Parser\n\
inner = lazy (\\_ -> top)\n\
\n\
leaf : Parser\n\
leaf = Parser (\\i -> i + 1)\n\
\n\
main = String.fromInt (run top 41)";
    assert_eq!(run(program), "42");
}

// Elm.Kernel.MJS — elm-explorations/linear-algebra runtime. The Math.Vector*
// / Math.Matrix4 Elm modules live in a package the unit-test harness cannot
// resolve, so this drives the ported `$Elm$Kernel$MJS$*` functions directly
// inside the runtime prelude and pins their output to the values a stock-elm
// build of the same operations produces (verified byte-for-byte).
#[test]
fn mjs_linear_algebra_kernel() {
    let script = format!(
        "{}\n\
         var out = [];\n\
         var a3 = A3($Elm$Kernel$MJS$v3, 1.0, 2.0, 3.0);\n\
         var b3 = A3($Elm$Kernel$MJS$v3, 4.0, 5.0, 6.0);\n\
         var n = $Elm$Kernel$MJS$v3normalize(a3);\n\
         out.push($Elm$Kernel$MJS$v3getX(n) + ',' + $Elm$Kernel$MJS$v3getY(n) + ',' + $Elm$Kernel$MJS$v3getZ(n));\n\
         out.push(String($Elm$Kernel$MJS$v3length(a3)));\n\
         out.push(String(A2($Elm$Kernel$MJS$v3distance, a3, b3)));\n\
         out.push(String(A2($Elm$Kernel$MJS$v3dot, a3, b3)));\n\
         var c = A2($Elm$Kernel$MJS$v3cross, a3, b3);\n\
         out.push($Elm$Kernel$MJS$v3getX(c) + ',' + $Elm$Kernel$MJS$v3getY(c) + ',' + $Elm$Kernel$MJS$v3getZ(c));\n\
         var rot = A2($Elm$Kernel$MJS$m4x4makeRotate, 1.2345, A3($Elm$Kernel$MJS$v3, 0.3, 0.4, 0.5));\n\
         var rn = $Elm$Kernel$MJS$m4x4toRecord(rot);\n\
         out.push(rn.m11 + ',' + rn.m22 + ',' + rn.m33 + ',' + rn.m21);\n\
         var persp = A4($Elm$Kernel$MJS$m4x4makePerspective, 45.0, 1.5, 0.1, 100.0);\n\
         var mul = A2($Elm$Kernel$MJS$m4x4mul, rot, persp);\n\
         var tr = A2($Elm$Kernel$MJS$v3mul4x4, mul, a3);\n\
         out.push($Elm$Kernel$MJS$v3getX(tr) + ',' + $Elm$Kernel$MJS$v3getY(tr) + ',' + $Elm$Kernel$MJS$v3getZ(tr));\n\
         var inv = $Elm$Kernel$MJS$m4x4inverse(persp);\n\
         var im = $Elm$Kernel$MJS$m4x4toRecord(inv.a);\n\
         out.push(im.m11 + ',' + im.m34);\n\
         out.push(String($Elm$Kernel$MJS$v2length(A2($Elm$Kernel$MJS$v2, 3.0, 4.0))));\n\
         out.push(String(A2($Elm$Kernel$MJS$v4dot, A4($Elm$Kernel$MJS$v4, 1.0, 2.0, 3.0, 4.0), A4($Elm$Kernel$MJS$v4, 5.0, 6.0, 7.0, 8.0))));\n\
         console.log(out.join('|'));",
        alm_compiler::generate::RUNTIME
    );
    let out = common::run_node(&script, alm_compiler::generate::RUNTIME);
    assert_eq!(
        out,
        "0.2672612419124244,0.5345224838248488,0.8017837257372732|\
         3.7416573867739413|5.196152422706632|32|-3,6,-3|\
         0.4505943892964256,0.5443953472214261,0.6649965788392839,0.8282986518453248|\
         1.3592938018696257,-1.462169205020312,-0.18658122328552051|\
         0.6213203435596425,-1|5|70"
    );
}
