//! elm/bytes round-trip on the native (typed/monomorphized) and wasm
//! backends. A compact, faithful `elm/bytes` package is resolved from a fake
//! ELM_HOME; the same program is compiled with `--target=native-typed` (run as
//! a binary) and `--target=wasm` (run under node's WASI). Both must print
//! exactly what the JS backend / official Elm 0.19.1 + elm/bytes 1.0.8 does,
//! covering signed/unsigned int 8/16/32 in both endiannesses, float32/64,
//! multi-byte UTF-8 strings, `bytes`, `Bytes.Decode.loop`, `map`/`map3`,
//! `andThen`, width/getStringWidth and decode failure.
//!
//! The `Bytes.Decode` combinators here destructure `Decoder` with `case`
//! rather than constructor-pattern function parameters (which the typed
//! backend does not yet compile); the semantics are identical to the stock
//! package.


mod common;

use std::process::Command;
use std::sync::Mutex;

use alm_compiler::{generate, project};

static ELM_HOME_LOCK: Mutex<()> = Mutex::new(());

// The `ø` is a real multi-byte UTF-8 scalar: it exercises string width and
// UTF-8 decode/round-trip, and must survive byte-for-byte.
const EXPECTED: &str =
    "(18,Just (65,1000000,3.5),(Just \"br\u{f8}d\",Just [7,8,9],(Nothing,Nothing,Just 3)))";

const BYTES_ELM: &str = r#"module Bytes exposing (Bytes, width, Endianness(..), getHostEndianness)

import Elm.Kernel.Bytes
import Task exposing (Task)


type Bytes
    = Bytes


width : Bytes -> Int
width =
    Elm.Kernel.Bytes.width


type Endianness
    = LE
    | BE


getHostEndianness : Task x Endianness
getHostEndianness =
    Elm.Kernel.Bytes.getHostEndianness LE BE
"#;
const ENCODE_ELM: &str = r#"module Bytes.Encode exposing
    ( encode, Encoder
    , signedInt8, signedInt16, signedInt32
    , unsignedInt8, unsignedInt16, unsignedInt32
    , float32, float64, bytes, string, getStringWidth, sequence
    )

import Bytes exposing (Bytes, Endianness(..))


type Encoder
    = I8 Int
    | I16 Endianness Int
    | I32 Endianness Int
    | U8 Int
    | U16 Endianness Int
    | U32 Endianness Int
    | F32 Endianness Float
    | F64 Endianness Float
    | Seq Int (List Encoder)
    | Utf8 Int String
    | Bytes Bytes


encode : Encoder -> Bytes
encode =
    Elm.Kernel.Bytes.encode


signedInt8 : Int -> Encoder
signedInt8 =
    I8


signedInt16 : Endianness -> Int -> Encoder
signedInt16 =
    I16


signedInt32 : Endianness -> Int -> Encoder
signedInt32 =
    I32


unsignedInt8 : Int -> Encoder
unsignedInt8 =
    U8


unsignedInt16 : Endianness -> Int -> Encoder
unsignedInt16 =
    U16


unsignedInt32 : Endianness -> Int -> Encoder
unsignedInt32 =
    U32


float32 : Endianness -> Float -> Encoder
float32 =
    F32


float64 : Endianness -> Float -> Encoder
float64 =
    F64


bytes : Bytes -> Encoder
bytes =
    Bytes


string : String -> Encoder
string str =
    Utf8 (Elm.Kernel.Bytes.getStringWidth str) str


getStringWidth : String -> Int
getStringWidth =
    Elm.Kernel.Bytes.getStringWidth


sequence : List Encoder -> Encoder
sequence builders =
    Seq (getWidths 0 builders) builders


getWidth : Encoder -> Int
getWidth builder =
    case builder of
        I8 _ ->
            1

        I16 _ _ ->
            2

        I32 _ _ ->
            4

        U8 _ ->
            1

        U16 _ _ ->
            2

        U32 _ _ ->
            4

        F32 _ _ ->
            4

        F64 _ _ ->
            8

        Seq w _ ->
            w

        Utf8 w _ ->
            w

        Bytes bs ->
            Elm.Kernel.Bytes.width bs


getWidths : Int -> List Encoder -> Int
getWidths w builders =
    case builders of
        [] ->
            w

        b :: bs ->
            getWidths (w + getWidth b) bs
"#;
const DECODE_ELM: &str = r#"module Bytes.Decode exposing
    ( Decoder, decode
    , signedInt8, signedInt16, signedInt32
    , unsignedInt8, unsignedInt16, unsignedInt32
    , float32, float64, string, bytes
    , map, map2, map3, map4, map5
    , andThen, succeed, fail
    , Step(..), loop
    )

import Bytes exposing (Bytes, Endianness(..))


type Decoder a
    = Decoder (Bytes -> Int -> ( Int, a ))


decode : Decoder a -> Bytes -> Maybe a
decode d bs =
    case d of
        Decoder decoder ->
            Elm.Kernel.Bytes.decode decoder bs


signedInt8 : Decoder Int
signedInt8 =
    Decoder Elm.Kernel.Bytes.read_i8


signedInt16 : Endianness -> Decoder Int
signedInt16 endianness =
    Decoder (Elm.Kernel.Bytes.read_i16 (endianness == LE))


signedInt32 : Endianness -> Decoder Int
signedInt32 endianness =
    Decoder (Elm.Kernel.Bytes.read_i32 (endianness == LE))


unsignedInt8 : Decoder Int
unsignedInt8 =
    Decoder Elm.Kernel.Bytes.read_u8


unsignedInt16 : Endianness -> Decoder Int
unsignedInt16 endianness =
    Decoder (Elm.Kernel.Bytes.read_u16 (endianness == LE))


unsignedInt32 : Endianness -> Decoder Int
unsignedInt32 endianness =
    Decoder (Elm.Kernel.Bytes.read_u32 (endianness == LE))


float32 : Endianness -> Decoder Float
float32 endianness =
    Decoder (Elm.Kernel.Bytes.read_f32 (endianness == LE))


float64 : Endianness -> Decoder Float
float64 endianness =
    Decoder (Elm.Kernel.Bytes.read_f64 (endianness == LE))


bytes : Int -> Decoder Bytes
bytes n =
    Decoder (Elm.Kernel.Bytes.read_bytes n)


string : Int -> Decoder String
string n =
    Decoder (Elm.Kernel.Bytes.read_string n)


map : (a -> b) -> Decoder a -> Decoder b
map func d =
    case d of
        Decoder decodeA ->
            Decoder
                (\bites offset ->
                    let
                        ( aOffset, a ) =
                            decodeA bites offset
                    in
                    ( aOffset, func a )
                )


map2 : (a -> b -> r) -> Decoder a -> Decoder b -> Decoder r
map2 func da db =
    case da of
        Decoder decodeA ->
            case db of
                Decoder decodeB ->
                    Decoder
                        (\bites offset ->
                            let
                                ( aOffset, a ) =
                                    decodeA bites offset

                                ( bOffset, b ) =
                                    decodeB bites aOffset
                            in
                            ( bOffset, func a b )
                        )


map3 : (a -> b -> c -> r) -> Decoder a -> Decoder b -> Decoder c -> Decoder r
map3 func da db dc =
    case da of
        Decoder decodeA ->
            case db of
                Decoder decodeB ->
                    case dc of
                        Decoder decodeC ->
                            Decoder
                                (\bites offset ->
                                    let
                                        ( aOffset, a ) =
                                            decodeA bites offset

                                        ( bOffset, b ) =
                                            decodeB bites aOffset

                                        ( cOffset, c ) =
                                            decodeC bites bOffset
                                    in
                                    ( cOffset, func a b c )
                                )


map4 : (a -> b -> c -> d -> r) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder r
map4 func da db dc dd =
    case da of
        Decoder decodeA ->
            case db of
                Decoder decodeB ->
                    case dc of
                        Decoder decodeC ->
                            case dd of
                                Decoder decodeD ->
                                    Decoder
                                        (\bites offset ->
                                            let
                                                ( aOffset, a ) =
                                                    decodeA bites offset

                                                ( bOffset, b ) =
                                                    decodeB bites aOffset

                                                ( cOffset, c ) =
                                                    decodeC bites bOffset

                                                ( dOffset, d ) =
                                                    decodeD bites cOffset
                                            in
                                            ( dOffset, func a b c d )
                                        )


map5 : (a -> b -> c -> d -> e -> r) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder e -> Decoder r
map5 func da db dc dd de =
    case da of
        Decoder decodeA ->
            case db of
                Decoder decodeB ->
                    case dc of
                        Decoder decodeC ->
                            case dd of
                                Decoder decodeD ->
                                    case de of
                                        Decoder decodeE ->
                                            Decoder
                                                (\bites offset ->
                                                    let
                                                        ( aOffset, a ) =
                                                            decodeA bites offset

                                                        ( bOffset, b ) =
                                                            decodeB bites aOffset

                                                        ( cOffset, c ) =
                                                            decodeC bites bOffset

                                                        ( dOffset, d ) =
                                                            decodeD bites cOffset

                                                        ( eOffset, e ) =
                                                            decodeE bites dOffset
                                                    in
                                                    ( eOffset, func a b c d e )
                                                )


andThen : (a -> Decoder b) -> Decoder a -> Decoder b
andThen callback d =
    case d of
        Decoder decodeA ->
            Decoder
                (\bites offset ->
                    let
                        ( newOffset, a ) =
                            decodeA bites offset
                    in
                    case callback a of
                        Decoder decodeB ->
                            decodeB bites newOffset
                )


succeed : a -> Decoder a
succeed a =
    Decoder (\_ offset -> ( offset, a ))


fail : Decoder a
fail =
    Decoder Elm.Kernel.Bytes.decodeFailure


type Step state a
    = Loop state
    | Done a


loop : state -> (state -> Decoder (Step state a)) -> Decoder a
loop state callback =
    Decoder (loopHelp state callback)


loopHelp : state -> (state -> Decoder (Step state a)) -> Bytes -> Int -> ( Int, a )
loopHelp state callback bites offset =
    case callback state of
        Decoder decoder ->
            let
                ( newOffset, step ) =
                    decoder bites offset
            in
            case step of
                Loop newState ->
                    loopHelp newState callback bites newOffset

                Done result ->
                    ( newOffset, result )
"#;
const MAIN_ELM: &str = r#"module Main exposing (main)

import Bytes exposing (Endianness(..))
import Bytes.Encode as E
import Bytes.Decode as D


listDecoder : D.Decoder (List Int)
listDecoder =
    D.unsignedInt8 |> D.andThen (\n -> D.loop ( n, [] ) step)


step : ( Int, List Int ) -> D.Decoder (D.Step ( Int, List Int ) (List Int))
step pair =
    let
        ( n, xs ) =
            pair
    in
    if n <= 0 then
        D.succeed (D.Done (List.reverse xs))

    else
        D.map (\x -> D.Loop ( n - 1, x :: xs )) D.unsignedInt8


main : String
main =
    let
        e =
            E.encode (E.sequence [ E.unsignedInt8 65, E.signedInt32 BE 1000000, E.float64 LE 3.5, E.string "brød" ])

        d =
            D.decode (D.map3 (\a b c -> ( a, b, c )) D.unsignedInt8 (D.signedInt32 BE) (D.float64 LE)) e

        s =
            D.decode (D.string 5) (E.encode (E.string "brød"))

        loopEnc =
            E.encode (E.sequence (E.unsignedInt8 3 :: List.map E.unsignedInt8 [ 7, 8, 9 ]))

        loopDec =
            D.decode listDecoder loopEnc

        fail =
            D.decode (D.unsignedInt32 BE) (E.encode (E.unsignedInt8 1))

        -- A failed read must abort the decode non-locally (JS throws; native
        -- longjmps): `map2`/`andThen` apply callbacks unconditionally, so a
        -- sentinel return would hand the failure dummy to `pairSum`, which
        -- destructures it as a tuple (pre-fix: SIGSEGV at 0x1).
        pairSum =
            D.map2 (\a b -> ( a, b )) D.unsignedInt8 D.unsignedInt8
                |> D.andThen (\( a, b ) -> D.succeed (a + b))

        failMid =
            D.decode pairSum (E.encode (E.unsignedInt8 1))

        okPair =
            D.decode pairSum (E.encode (E.sequence [ E.unsignedInt8 1, E.unsignedInt8 2 ]))
    in
    Debug.toString ( Bytes.width e, d, ( s, loopDec, ( fail, failMid, okPair ) ) )
"#;

fn setup() -> common::TestDir {
    let dir = common::test_dir("alm-bytes-native", "t");
    let src = dir.join("src");
    let pkg = dir.join("elm-home/0.19.1/packages/elm/bytes/1.0.8/src");
    std::fs::create_dir_all(src.join("")).unwrap();
    std::fs::create_dir_all(pkg.join("Bytes")).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"], "dependencies": { "direct": { "elm/bytes": "1.0.8" }, "indirect": {} }, "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.parent().unwrap().join("elm.json"),
        r#"{ "type": "package", "name": "elm/bytes", "summary": "b", "license": "BSD-3-Clause", "version": "1.0.8", "exposed-modules": ["Bytes", "Bytes.Encode", "Bytes.Decode"], "elm-version": "0.19.0 <= v < 0.20.0", "dependencies": { "elm/core": "1.0.0 <= v < 2.0.0" }, "test-dependencies": {} }"#,
    )
    .unwrap();
    std::fs::write(pkg.join("Bytes.elm"), BYTES_ELM).unwrap();
    std::fs::write(pkg.join("Bytes/Encode.elm"), ENCODE_ELM).unwrap();
    std::fs::write(pkg.join("Bytes/Decode.elm"), DECODE_ELM).unwrap();
    std::fs::write(src.join("Main.elm"), MAIN_ELM).unwrap();
    dir
}

fn expected() -> &'static str {
    EXPECTED
}

#[test]
fn bytes_roundtrip_native_typed() {
    let _guard = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = setup();
    std::env::set_var("ELM_HOME", dir.join("elm-home"));
    let binary = dir.join("main");
    let res = project::compile_project_typed(
        &dir.join("src/Main.elm"),
        &binary,
        generate::native::Target::Native,
        generate::native::OptLevel::Release,
    );
    std::env::remove_var("ELM_HOME");
    res.unwrap_or_else(|errs| {
        panic!(
            "typed native build failed:\n{}",
            errs.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
        )
    });
    let output = Command::new(&binary).output().expect("run binary");
    assert!(
        output.status.success(),
        "binary failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        expected()
    );
}

#[test]
fn bytes_roundtrip_wasm() {
    let _guard = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = setup();
    std::env::set_var("ELM_HOME", dir.join("elm-home"));
    let wasm = dir.join("main.wasm");
    let res = project::compile_project_typed(
        &dir.join("src/Main.elm"),
        &wasm,
        generate::native::Target::Wasm,
        generate::native::OptLevel::Release,
    );
    std::env::remove_var("ELM_HOME");
    res.unwrap_or_else(|errs| {
        panic!(
            "typed wasm build failed:\n{}",
            errs.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
        )
    });
    let runner = dir.join("run.cjs");
    std::fs::write(
        &runner,
        format!(
            "const {{WASI}}=require('node:wasi');const fs=require('fs');(async()=>{{\
             const wasi=new WASI({{version:'preview1',args:['p'],env:{{}},returnOnExit:true}});\
             const m=await WebAssembly.compile(fs.readFileSync({:?}));\
             const imports=Object.assign({{env:new Proxy({{}},{{get:()=>()=>0}})}},wasi.getImportObject());\
             const i=await WebAssembly.instantiate(m,imports);\
             wasi.start(i);}})();",
            wasm.display()
        ),
    )
    .unwrap();
    let output = Command::new("node")
        .arg("--no-warnings")
        .arg(&runner)
        .env_remove("FORCE_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1")
        .output()
        .expect("spawn node");
    assert!(
        output.status.success(),
        "wasm run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim_end(),
        expected()
    );
}
