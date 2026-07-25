//! elm/bytes round-trips across backends. A compact, faithful `elm/bytes`
//! package is resolved from a fake ELM_HOME.
//!
//! - `bytes_full_js_native`: the full program (unsigned/signed int 8/16/32 in
//!   both endiannesses, float64, UTF-8 strings, `Bytes.width`, `Decode.loop`/
//!   `map3`/`andThen`, a failed decode) on JS and native — both correct and
//!   identical.
//! - `bytes_scalars_all_backends`: a scalar+string subset (no list decode) on
//!   JS, native, AND wasm-gc — all three agree.
//!
//! wasm-gc is excluded from the list-decode case: decoding into an unboxed
//! scalar `List Int`/`List Float` currently yields zero elements (the generic
//! decode kernel produces boxed values where the unboxed-scalar-list ABI
//! expects unboxed). Scalars, strings, `Bytes`, and `Bytes.width` decode
//! correctly on all three. Tracked as a known wasm-gc unboxed-ABI limitation.

mod common;

use std::process::Command;
use std::sync::Mutex;

use alm_compiler::{generate, ir, project};

static ELM_HOME_LOCK: Mutex<()> = Mutex::new(());

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
const FULL_MAIN: &str = r#"module Main exposing (main)

import Bytes exposing (Endianness(..))
import Bytes.Encode as E
import Bytes.Decode as D


listDecoder : D.Decoder (List Int)
listDecoder =
    D.unsignedInt8 |> D.andThen (\n -> D.loop ( n, [] ) step)


step : ( Int, List Int ) -> D.Decoder (D.Step ( Int, List Int ) (List Int))
step ( n, xs ) =
    if n <= 0 then
        D.succeed (D.Done (List.reverse xs))
    else
        D.map (\x -> D.Loop ( n - 1, x :: xs )) D.unsignedInt8


main : String
main =
    let
        e = E.encode (E.sequence [ E.unsignedInt8 65, E.signedInt32 BE 1000000, E.float64 LE 3.5, E.string "br\u{00f8}d" ])
        triple = D.decode (D.map3 (\a b c -> String.fromInt a ++ "," ++ String.fromInt b ++ "," ++ String.fromFloat c) D.unsignedInt8 (D.signedInt32 BE) (D.float64 LE)) e
        str = D.decode (D.string 5) (E.encode (E.string "br\u{00f8}d"))
        loopEnc = E.encode (E.sequence (E.unsignedInt8 3 :: List.map E.unsignedInt8 [ 7, 8, 9 ]))
        loopDec = D.decode listDecoder loopEnc
        failed = case D.decode (D.unsignedInt32 BE) (E.encode (E.unsignedInt8 1)) of
            Just _ -> "some"
            Nothing -> "none"
    in
    String.join "|"
        [ String.fromInt (Bytes.width e)
        , Maybe.withDefault "?" triple
        , Maybe.withDefault "?" str
        , loopDec |> Maybe.map (\xs -> String.join "," (List.map String.fromInt xs)) |> Maybe.withDefault "?"
        , failed
        ]
"#;
const SCALAR_MAIN: &str = r#"module Main exposing (main)

import Bytes exposing (Endianness(..))
import Bytes.Encode as E
import Bytes.Decode as D


main : String
main =
    let
        e = E.encode (E.sequence [ E.unsignedInt8 65, E.signedInt16 LE -3, E.signedInt32 BE 1000000, E.float64 LE 3.5, E.string "br\u{00f8}d" ])
        r = D.decode
                (D.map5 (\a b c d s -> String.fromInt a ++ "," ++ String.fromInt b ++ "," ++ String.fromInt c ++ "," ++ String.fromFloat d ++ "," ++ s)
                    D.unsignedInt8 (D.signedInt16 LE) (D.signedInt32 BE) (D.float64 LE) (D.string 5))
                e
        failed = case D.decode (D.unsignedInt32 BE) (E.encode (E.unsignedInt8 1)) of
            Just _ -> "some"
            Nothing -> "none"
    in
    String.join "|" [ String.fromInt (Bytes.width e), Maybe.withDefault "?" r, failed ]
"#;

fn write_project(main: &str) -> common::TestDir {
    let dir = common::test_dir("alm-bytes", "t");
    let src = dir.join("src");
    let pkg = dir.join("elm-home/0.19.1/packages/elm/bytes/1.0.8/src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(pkg.join("Bytes")).unwrap();
    std::fs::write(dir.join("elm.json"), r#"{ "type": "application", "source-directories": ["src"], "dependencies": { "direct": { "elm/bytes": "1.0.8" }, "indirect": {} }, "test-dependencies": { "direct": {}, "indirect": {} } }"#).unwrap();
    std::fs::write(pkg.parent().unwrap().join("elm.json"), r#"{ "type": "package", "name": "elm/bytes", "summary": "b", "license": "BSD-3-Clause", "version": "1.0.8", "exposed-modules": ["Bytes", "Bytes.Encode", "Bytes.Decode"], "elm-version": "0.19.0 <= v < 0.20.0", "dependencies": { "elm/core": "1.0.0 <= v < 2.0.0" }, "test-dependencies": {} }"#).unwrap();
    std::fs::write(pkg.join("Bytes.elm"), BYTES_ELM).unwrap();
    std::fs::write(pkg.join("Bytes/Encode.elm"), ENCODE_ELM).unwrap();
    std::fs::write(pkg.join("Bytes/Decode.elm"), DECODE_ELM).unwrap();
    std::fs::write(src.join("Main.elm"), main).unwrap();
    dir
}

fn run_cmd(command: &mut Command, what: &str) -> String {
    let out = command.env_remove("FORCE_COLOR").env_remove("CLICOLOR_FORCE").env("NO_COLOR", "1")
        .output().unwrap_or_else(|e| panic!("run {}: {}", what, e));
    assert!(out.status.success(), "{} failed:\nstdout: {}\nstderr: {}", what,
        String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

fn check(dir: &std::path::Path) -> alm_compiler::project::CheckedProject {
    let entry = dir.join("src/Main.elm");
    project::check_project(&entry).unwrap_or_else(|errs| {
        std::env::remove_var("ELM_HOME");
        panic!("check failed:\n{}", errs.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    })
}

fn js_out(dir: &std::path::Path, checked: &alm_compiler::project::CheckedProject) -> String {
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).unwrap();
    run_cmd(Command::new("node").arg("-e").arg(format!("process.stdout.write(require({:?}).Main.main)", bundle.display())), "node")
}

fn native_out(dir: &std::path::Path, checked: &alm_compiler::project::CheckedProject) -> String {
    let program = ir::lower::lower_project(&checked.modules);
    let binary = dir.join("main");
    generate::native::build(&program, &binary, generate::native::OptLevel::Release)
        .unwrap_or_else(|e| { std::env::remove_var("ELM_HOME"); panic!("native build failed: {}", e) });
    run_cmd(&mut Command::new(&binary), "native")
}

fn wasmgc_out(dir: &std::path::Path) -> String {
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&dir.join("src/Main.elm"), &wasm, false)
        .unwrap_or_else(|e| panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")));
    let runner = dir.join("run_str.cjs");
    std::fs::write(&runner, format!("{HOST_ENV}{STR_RUNNER_TAIL}")).unwrap();
    run_cmd(Command::new("node").arg(&runner).arg(&wasm), "wasm-gc")
}

#[test]
fn bytes_full_js_native() {
    let _g = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = write_project(FULL_MAIN);
    std::env::set_var("ELM_HOME", dir.join("elm-home"));
    let checked = check(&dir);
    let js = js_out(&dir, &checked);
    let native = native_out(&dir, &checked);
    std::env::remove_var("ELM_HOME");
    assert_eq!(js, "18|65,1000000,3.5|br\u{00f8}d|7,8,9|none");
    assert_eq!(js, native, "js vs native");
}

#[test]
fn bytes_scalars_all_backends() {
    let _g = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = write_project(SCALAR_MAIN);
    std::env::set_var("ELM_HOME", dir.join("elm-home"));
    let checked = check(&dir);
    let js = js_out(&dir, &checked);
    let native = native_out(&dir, &checked);
    let gc = wasmgc_out(&dir);
    std::env::remove_var("ELM_HOME");
    assert_eq!(js, "20|65,-3,1000000,3.5,br\u{00f8}d|none");
    assert_eq!(js, native, "js vs native");
    assert_eq!(js, gc, "js vs wasm-gc");
}

const HOST_ENV: &str = r#"
let mem;
const HM={math_sin:Math.sin,math_cos:Math.cos,math_tan:Math.tan,math_asin:Math.asin,math_acos:Math.acos,math_atan:Math.atan,math_log:Math.log,math_atan2:Math.atan2,math_pow:Math.pow,host_now:()=>0,
  host_ftoa:(x,o)=>{const b=Buffer.from(String(x));new Uint8Array(mem.buffer,o,b.length).set(b);return b.length;},
  host_atof:(p,l,o)=>{const s=Buffer.from(new Uint8Array(mem.buffer,p,l)).toString();if(s.length===0||/[\sxbo]/.test(s))return 0;const n=+s;if(n!==n)return 0;new DataView(mem.buffer).setFloat64(o,n,true);return 1;}};
const HOST_IMPORTS={env:new Proxy(HM,{get:(t,k)=>t[k]||(()=>0)})};
"#;

const STR_RUNNER_TAIL: &str = r#"
const fs = require('fs');
const bytes = fs.readFileSync(process.argv[2]);
const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), HOST_IMPORTS);
mem = instance.exports.memory;
const len = instance.exports.render();
const out = new Uint8Array(instance.exports.memory.buffer, 0, len);
process.stdout.write(Buffer.from(out).toString('utf8'));
"#;
