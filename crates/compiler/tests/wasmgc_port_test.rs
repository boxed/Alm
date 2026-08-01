//! Outgoing ports on the wasm-gc backend carry a raw Elm value that the runtime
//! must convert to a `Json.Value` (via a type-directed encoder) before handing a
//! JSON string to the host. This checks that conversion for every port-legal
//! payload shape by compiling one worker to BOTH the JS and wasm-gc backends,
//! capturing what each emits through its ports, and asserting they agree.
//!
//! JSON object key order is not significant (and differs by backend: JS keeps
//! source order, wasm-gc uses the runtime's alphabetical field layout), so both
//! sides are canonicalized with recursively-sorted keys before comparison.

mod common;

use std::process::Command;

use alm_compiler::{generate, project};

const MAIN: &str = r#"port module Main exposing (main)

type Msg = Go

port outStr : String -> Cmd msg
port outInt : Int -> Cmd msg
port outFloat : Float -> Cmd msg
port outBool : Bool -> Cmd msg
port outUnit : () -> Cmd msg
port outList : List Int -> Cmd msg
port outListStr : List String -> Cmd msg
port outRec : { name : String, n : Int, ok : Bool } -> Cmd msg
port outNested : { xs : List Int, inner : { a : Bool } } -> Cmd msg
port outTup2 : ( Int, String ) -> Cmd msg
port outTup3 : ( Int, String, Bool ) -> Cmd msg
port outMaybe : Maybe Int -> Cmd msg

main : Program () () Msg
main =
    Platform.worker
        { init = \_ ->
            ( ()
            , Cmd.batch
                [ outStr "hello \"world\""
                , outInt 42
                , outFloat 3.0
                , outBool True
                , outUnit ()
                , outList [ 1, 2, 3 ]
                , outListStr [ "a", "b" ]
                , outRec { name = "bob", n = 7, ok = False }
                , outNested { xs = [ 4, 5 ], inner = { a = True } }
                , outTup2 ( 9, "x" )
                , outTup3 ( 1, "y", False )
                , outMaybe (Just 5)
                , outMaybe Nothing
                ]
            )
        , update = \_ model -> ( model, Cmd.none )
        , subscriptions = \_ -> Sub.none
        }
"#;

// Recursively sorts object keys so the two backends' JSON compares equal
// regardless of key order, then prints `name = <canonical json>` per emission.
const CANON: &str = r#"
function canon(v){
  if (Array.isArray(v)) return v.map(canon);
  if (v && typeof v === 'object'){
    const o = {};
    for (const k of Object.keys(v).sort()) o[k] = canon(v[k]);
    return o;
  }
  return v;
}
"#;

const PORTS: &str = r#"["outStr","outInt","outFloat","outBool","outUnit","outList","outListStr","outRec","outNested","outTup2","outTup3","outMaybe"]"#;

fn js_runner(bundle: &std::path::Path, ports: &str) -> String {
    format!(
        r#"{CANON}
const app = require({:?}).Elm.Main.init({{}});
for (const p of {ports}) {{
  if (app.ports && app.ports[p]) app.ports[p].subscribe(v => console.log(p + " = " + JSON.stringify(canon(v))));
}}
"#,
        bundle.display()
    )
}

const WASM_RUNNER: &str = r#"
let mem;
const HM={math_sin:Math.sin,math_cos:Math.cos,math_tan:Math.tan,math_asin:Math.asin,math_acos:Math.acos,math_atan:Math.atan,math_log:Math.log,math_atan2:Math.atan2,math_pow:Math.pow,host_now:()=>0,
  host_ftoa:(x,o)=>{const b=Buffer.from(String(x));new Uint8Array(mem.buffer,o,b.length).set(b);return b.length;},
  host_atof:(p,l,o)=>{const s=Buffer.from(new Uint8Array(mem.buffer,p,l)).toString();if(s.length===0||/[\sxbo]/.test(s))return 0;const n=+s;if(n!==n)return 0;new DataView(mem.buffer).setFloat64(o,n,true);return 1;},
  host_port_out:(np,nl,jp,jl)=>{const name=Buffer.from(new Uint8Array(mem.buffer,np,nl)).toString();const j=Buffer.from(new Uint8Array(mem.buffer,jp,jl)).toString();console.log(name + " = " + JSON.stringify(canon(JSON.parse(j))));}};
const fs=require('fs');
const bytes=fs.readFileSync(process.argv[2]);
const instance=new WebAssembly.Instance(new WebAssembly.Module(bytes),{env:new Proxy(HM,{get:(t,k)=>t[k]||(()=>0)})});
mem=instance.exports.memory;
instance.exports.alm_browser_start();
"#;

fn node(args: &[&std::ffi::OsStr]) -> String {
    let out = Command::new("node")
        .args(args)
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .output()
        .expect("run node");
    assert!(
        out.status.success(),
        "node failed:\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim_end().to_string()
}

#[test]
fn outgoing_ports_encode_like_js() {
    let dir = common::test_dir("alm-wasmgc-port", "outgoing");
    let entry = dir.join("Main.elm");
    std::fs::write(&entry, MAIN).expect("write Main.elm");

    // JS backend.
    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("write bundle");
    let js_runner_path = dir.join("runjs.cjs");
    std::fs::write(&js_runner_path, js_runner(&bundle, PORTS)).expect("write js runner");
    let js_out = node(&[js_runner_path.as_os_str()]);

    // wasm-gc backend.
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm, false).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let wasm_runner = dir.join("runwasm.cjs");
    std::fs::write(&wasm_runner, format!("{CANON}{WASM_RUNNER}")).expect("write wasm runner");
    let wasm_out = node(&[wasm_runner.as_os_str(), wasm.as_os_str()]);

    assert_eq!(js_out, wasm_out, "JS and wasm-gc port output differ");
    // Sanity: every port actually fired (13 emissions: outMaybe fires twice).
    assert_eq!(wasm_out.lines().count(), 13, "unexpected emission count:\n{wasm_out}");
}

// A port declared in one module and used from another, which is how a real app
// of any size is laid out (exosphere keeps all of its in `Ports`). A port has no
// definition to specialize, so monomorphization must leave a cross-module use
// under its plain name rather than mangle it into a specialization that never
// exists — and the back end must recognize the port both applied and used as a
// bare value (`List.map outRec`, which needs a closure over something that has
// no function to close over).
const PORTS_MODULE: &str = r#"port module Ports exposing (inbound, outInt, outRec, outStr)

import Json.Encode


port outStr : String -> Cmd msg


port outInt : Int -> Cmd msg


port outRec : { n : Int, ok : Bool } -> Cmd msg


port inbound : (Json.Encode.Value -> msg) -> Sub msg
"#;

const CROSS_MODULE_MAIN: &str = r#"module Main exposing (main)

import Json.Encode
import Platform
import Ports


type Msg
    = Got Json.Encode.Value


main : Program () () Msg
main =
    Platform.worker
        { init =
            \_ ->
                ( ()
                , Cmd.batch
                    -- Applied directly, across the module boundary.
                    [ Ports.outStr "hi"

                    -- Used as a value: passed to a HOF, and piped into.
                    , Cmd.batch (List.map Ports.outRec [ { n = 1, ok = True }, { n = 2, ok = False } ])
                    , 7 |> Ports.outInt
                    ]
                )
        , update = \_ model -> ( model, Cmd.none )

        -- An incoming port, also across the module boundary.
        , subscriptions = \_ -> Ports.inbound Got
        }
"#;

const CROSS_MODULE_PORT_NAMES: &str = r#"["outStr","outInt","outRec"]"#;

#[test]
fn cross_module_ports_encode_like_js() {
    let dir = common::test_dir("alm-wasmgc-port", "cross-module");
    std::fs::write(dir.join("Ports.elm"), PORTS_MODULE).expect("write Ports.elm");
    let entry = dir.join("Main.elm");
    std::fs::write(&entry, CROSS_MODULE_MAIN).expect("write Main.elm");

    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("write bundle");
    let js_runner_path = dir.join("runjs.cjs");
    std::fs::write(&js_runner_path, js_runner(&bundle, CROSS_MODULE_PORT_NAMES))
        .expect("write js runner");
    let js_out = node(&[js_runner_path.as_os_str()]);

    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm, false).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let wasm_runner = dir.join("runwasm.cjs");
    std::fs::write(&wasm_runner, format!("{CANON}{WASM_RUNNER}")).expect("write wasm runner");
    let wasm_out = node(&[wasm_runner.as_os_str(), wasm.as_os_str()]);

    assert_eq!(js_out, wasm_out, "JS and wasm-gc cross-module port output differ");
    // outStr, outRec twice, outInt.
    assert_eq!(wasm_out.lines().count(), 4, "unexpected emission count:\n{wasm_out}");
}
