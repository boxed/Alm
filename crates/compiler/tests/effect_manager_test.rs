//! User-defined `effect module`s run through alm's effect-manager protocol.
//! Stock elm restricts effect modules to the @elm organization; alm allows
//! them. Each program is a `Platform.worker` that observes the manager's
//! callback: on JS and native through `Terminal.writeLine` (outputs must agree),
//! and on wasm-gc through an outgoing port (no `Terminal` on that backend).

mod common;

use std::process::Command;

use alm_compiler::{generate, ir, project};

// --- effect modules (shared by every backend) -------------------------------

/// `command` -> onEffects -> sendToApp.
const TOAST_CMD: &str = r#"effect module Toast where { command = MyCmd } exposing (say)

import Task exposing (Task)

type MyCmd msg = Say msg

say : msg -> Cmd msg
say m = command (Say m)

cmdMap f (Say m) = Say (f m)

init = Task.succeed ()

onEffects router cmds state =
    case cmds of
        [] -> Task.succeed state
        (Say m) :: rest ->
            Task.andThen (\_ -> onEffects router rest state) (Platform.sendToApp router m)

onSelfMsg router m state = Task.succeed state
"#;

/// `command` -> onEffects -> sendToSelf -> onSelfMsg -> sendToApp.
const TOAST_ECHO: &str = r#"effect module Toast where { command = MyCmd } exposing (echo)

import Task exposing (Task)

type MyCmd msg = Echo msg

echo : msg -> Cmd msg
echo m = command (Echo m)

cmdMap f (Echo m) = Echo (f m)

init = Task.succeed 0

onEffects router cmds count =
    case cmds of
        [] -> Task.succeed count
        (Echo m) :: rest ->
            Task.andThen (\_ -> onEffects router rest (count + 1)) (Platform.sendToSelf router m)

onSelfMsg router m count =
    Task.andThen (\_ -> Task.succeed (count + 1)) (Platform.sendToApp router m)
"#;

/// `subscription` -> onEffects (with the current sub list) -> sendToApp. Fires
/// once and remembers it, so the app does not loop.
const TICKER: &str = r#"effect module Ticker where { subscription = MySub } exposing (listen)

import Task exposing (Task)

type MySub msg = Listen msg

listen : msg -> Sub msg
listen m = subscription (Listen m)

subMap f (Listen m) = Listen (f m)

init = Task.succeed False

onEffects router subs fired =
    case ( subs, fired ) of
        ( (Listen m) :: _, False ) ->
            Task.andThen (\_ -> Task.succeed True) (Platform.sendToApp router m)
        _ ->
            Task.succeed fired

onSelfMsg router m fired = Task.succeed fired
"#;

// --- JS + native harness ----------------------------------------------------

/// Write the modules into a scratch project, compile through both the JS and
/// native backends, run each `Main.main` (worker) to completion, and assert the
/// two backends print the same thing. Returns that shared output.
fn run(test_name: &str, modules: &[(&str, &str)]) -> String {
    let dir = common::test_dir("alm-effect-manager", test_name);
    for (file, source) in modules {
        std::fs::write(dir.join(file), source).expect("write module");
    }
    let entry = dir.join("Main.elm");
    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!(
            "check failed:\n{}",
            errors
                .iter()
                .map(|e| e.render())
                .collect::<Vec<_>>()
                .join("\n")
        )
    });

    // JS backend under node.
    let js = generate::generate_project(&checked.modules);
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, &js).expect("write bundle");
    let js_out = run_command(
        Command::new("node")
            .arg("-e")
            .arg(format!("require({:?})['Main']['main'].init({{}})", bundle.display())),
        "node",
        &js,
    );

    // Native backend (a real binary driven by the C event loop).
    let program = ir::lower::lower_project(&checked.modules);
    let binary = dir.join("prog");
    generate::native::build(&program, &binary, generate::native::OptLevel::Release)
        .unwrap_or_else(|e| panic!("native build failed: {}", e));
    let native_out = run_command(&mut Command::new(&binary), "native binary", &js);

    assert_eq!(js_out, native_out, "JS and native output differ");
    js_out
}

fn run_command(command: &mut Command, what: &str, js_for_error: &str) -> String {
    let output = command
        .env_remove("FORCE_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1")
        .output()
        .unwrap_or_else(|e| panic!("run {}: {}", what, e));
    assert!(
        output.status.success(),
        "{} failed:\nstderr: {}\n\nJS:\n{}",
        what,
        String::from_utf8_lossy(&output.stderr),
        js_for_error
    );
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

// --- wasm-gc harness --------------------------------------------------------

// Instantiates the module, wires the outgoing port to stdout (unwrapping the
// JSON string the port carries), and starts the worker. Math/`host_*` imports
// mirror the wasmgc_test host env; only `host_port_out` is added.
const WASMGC_PORT_RUNNER: &str = r#"
let mem;
const HM={math_sin:Math.sin,math_cos:Math.cos,math_tan:Math.tan,math_asin:Math.asin,math_acos:Math.acos,math_atan:Math.atan,math_log:Math.log,math_atan2:Math.atan2,math_pow:Math.pow,host_now:()=>0,
  host_ftoa:(x,o)=>{const b=Buffer.from(String(x));new Uint8Array(mem.buffer,o,b.length).set(b);return b.length;},
  host_atof:(p,l,o)=>{const s=Buffer.from(new Uint8Array(mem.buffer,p,l)).toString();if(s.length===0||/[\sxbo]/.test(s))return 0;const n=+s;if(n!==n)return 0;new DataView(mem.buffer).setFloat64(o,n,true);return 1;},
  host_port_out:(np,nl,jp,jl)=>{const j=Buffer.from(new Uint8Array(mem.buffer,jp,jl)).toString();process.stdout.write(JSON.parse(j));}};
const fs = require('fs');
const bytes = fs.readFileSync(process.argv[2]);
const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {env:new Proxy(HM,{get:(t,k)=>t[k]||(()=>0)})});
mem = instance.exports.memory;
instance.exports.alm_browser_start();
"#;

/// Compile the modules with the wasm-gc backend, run the worker under Node, and
/// return what it emitted through the outgoing port.
fn run_wasmgc(test_name: &str, modules: &[(&str, &str)]) -> String {
    let dir = common::test_dir("alm-effect-manager-wasmgc", test_name);
    for (file, source) in modules {
        std::fs::write(dir.join(file), source).expect("write module");
    }
    let entry = dir.join("Main.elm");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm, false).unwrap_or_else(|e| {
        panic!(
            "wasmgc build failed:\n{}",
            e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
        )
    });
    let runner = dir.join("run.cjs");
    std::fs::write(&runner, WASMGC_PORT_RUNNER).expect("write runner");
    let output = Command::new("node")
        .arg(&runner)
        .arg(&wasm)
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .output()
        .expect("run node");
    assert!(
        output.status.success(),
        "wasm-gc run failed:\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

/// A port-observing `Main` for the wasm-gc backend. `effect_cmd` is the
/// expression producing the initial command; `sub` the subscriptions body.
fn wasmgc_main(import: &str, effect_cmd: &str, sub: &str) -> String {
    format!(
        r#"port module Main exposing (main)

{import}
import Json.Encode as JE

port out : JE.Value -> Cmd msg

type Msg = Got String

main : Program () () Msg
main =
    Platform.worker
        {{ init = \_ -> ( (), {effect_cmd} )
        , update = \(Got s) model -> ( model, out (JE.string s) )
        , subscriptions = {sub}
        }}
"#
    )
}

// --- scenarios --------------------------------------------------------------

/// command -> onEffects -> sendToApp -> update.
#[test]
fn command_reaches_the_app() {
    let main = r#"module Main exposing (main)

import Toast

type Msg = Got String

main : Program () () Msg
main =
    Platform.worker
        { init = \_ -> ( (), Toast.say (Got "command-ok") )
        , update = \(Got s) model -> ( model, Terminal.writeLine s )
        , subscriptions = \_ -> Sub.none
        }
"#;
    assert_eq!(
        run("command", &[("Toast.elm", TOAST_CMD), ("Main.elm", main)]),
        "command-ok"
    );
}

#[test]
fn command_reaches_the_app_wasmgc() {
    let main = wasmgc_main("import Toast", r#"Toast.say (Got "command-ok")"#, "\\_ -> Sub.none");
    assert_eq!(
        run_wasmgc("command", &[("Toast.elm", TOAST_CMD), ("Main.elm", &main)]),
        "command-ok"
    );
}

/// command -> onEffects -> sendToSelf -> onSelfMsg -> sendToApp -> update.
#[test]
fn self_message_round_trip() {
    let main = r#"module Main exposing (main)

import Toast

type Msg = Got String

main : Program () () Msg
main =
    Platform.worker
        { init = \_ -> ( (), Toast.echo (Got "self-ok") )
        , update = \(Got s) model -> ( model, Terminal.writeLine s )
        , subscriptions = \_ -> Sub.none
        }
"#;
    assert_eq!(
        run("self", &[("Toast.elm", TOAST_ECHO), ("Main.elm", main)]),
        "self-ok"
    );
}

#[test]
fn self_message_round_trip_wasmgc() {
    let main = wasmgc_main("import Toast", r#"Toast.echo (Got "self-ok")"#, "\\_ -> Sub.none");
    assert_eq!(
        run_wasmgc("self", &[("Toast.elm", TOAST_ECHO), ("Main.elm", &main)]),
        "self-ok"
    );
}

/// subscription -> onEffects (with the current sub list) -> sendToApp. The
/// manager fires once and remembers it, so the app does not loop.
#[test]
fn subscription_reaches_the_app() {
    let main = r#"module Main exposing (main)

import Ticker

type Msg = Got String

main : Program () () Msg
main =
    Platform.worker
        { init = \_ -> ( (), Cmd.none )
        , update = \(Got s) model -> ( model, Terminal.writeLine s )
        , subscriptions = \_ -> Ticker.listen (Got "sub-ok")
        }
"#;
    assert_eq!(
        run("subscription", &[("Ticker.elm", TICKER), ("Main.elm", main)]),
        "sub-ok"
    );
}

#[test]
fn subscription_reaches_the_app_wasmgc() {
    let main = wasmgc_main("import Ticker", "Cmd.none", r#"\_ -> Ticker.listen (Got "sub-ok")"#);
    assert_eq!(
        run_wasmgc("subscription", &[("Ticker.elm", TICKER), ("Main.elm", &main)]),
        "sub-ok"
    );
}
