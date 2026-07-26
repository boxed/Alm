//! Random is a bundled effect module (not a builtin). Front-end check: a program
//! using Random generators + `Random.generate` type-checks through the bundled
//! source and Random carries an effect-manager declaration.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use alm_compiler::{generate, ir, project};

const ELM_JSON: &str = r#"{
    "type": "application",
    "source-directories": ["src"],
    "elm-version": "0.19.1",
    "dependencies": { "direct": { "elm/core": "1.0.5", "elm/random": "1.0.0", "elm/time": "1.0.0" }, "indirect": {} },
    "test-dependencies": { "direct": {}, "indirect": {} }
}"#;

const MAIN: &str = r#"module Main exposing (main)

import Platform
import Random


type alias Model = Int
type Msg = Rolled Int


main : Program () Model Msg
main =
    Platform.worker
        { init = \_ -> ( 0, Random.generate Rolled (Random.int 1 6) )
        , update = \msg _ -> case msg of
            Rolled n -> ( n, Cmd.none )
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn random_program_typechecks_through_bundled_effect_module() {
    let dir = common::test_dir("alm-random", "check");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), MAIN).unwrap();

    match project::check_project(&src.join("Main.elm")) {
        Ok(checked) => {
            let random = checked
                .modules
                .iter()
                .find(|m| m.name.as_str() == "Random")
                .expect("Random module should be compiled from bundled source");
            assert!(random.manager.is_some(), "Random should be an effect module (have a manager)");
        }
        Err(errors) => {
            let msg = errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n");
            panic!("Random program failed to type-check:\n{msg}");
        }
    }
}

/// Run `bin` with an out-of-band kill guard (never run a native binary
/// uncapped — a runaway once crashed the machine). Poll for exit up to `limit`.
fn run_capped(bin: &std::path::Path, limit: Duration) -> String {
    let mut child = Command::new(bin)
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn native binary");
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take().unwrap().read_to_string(&mut out).ok();
                assert!(status.success(), "native binary exited with {status}");
                return out.trim_end().to_string();
            }
            None => {
                if start.elapsed() > limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("native Random program did not stop within {limit:?}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

// A worker that on init rolls a die through the Random effect-manager protocol
// (`Random.generate` -> manager `init` seeds from `Time.now` -> `onEffects`
// steps the generator -> `sendToApp` -> update), prints the roll, then goes
// idle (Sub.none, Cmd.none) so the program exits on its own.
const DIE_MAIN: &str = r#"module Main exposing (main)

import Platform
import Random


type alias Model = Int
type Msg = Rolled Int


main : Program () Model Msg
main =
    Platform.worker
        { init = \_ -> ( 0, Random.generate Rolled (Random.int 1 6) )
        , update =
            \msg _ ->
                case msg of
                    Rolled n ->
                        ( n, Terminal.writeLine (String.fromInt n) )
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn random_generate_reaches_app_native() {
    let dir = common::test_dir("alm-random", "generate-native");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), DIE_MAIN).unwrap();

    let checked = project::check_project(&src.join("Main.elm")).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let program = ir::lower::lower_project(&checked.modules);
    let binary = dir.join("prog");
    generate::native::build(&program, &binary, generate::native::OptLevel::Release)
        .unwrap_or_else(|e| panic!("native build failed: {e}"));

    let out = run_capped(&binary, Duration::from_secs(30));
    let n: i64 = out.trim().parse().unwrap_or_else(|_| panic!("expected a die roll, got {out:?}"));
    assert!((1..=6).contains(&n), "Random.generate die roll {n} not in 1..6");
}

// `Random.step` is a pure primitive: a fixed seed must yield a fixed value, and
// the value must be byte-identical across the JS and native backends (same
// PCG algorithm and seed reps). Emit it through `Terminal.writeLine` on init.
const STEP_MAIN: &str = r#"module Main exposing (main)

import Platform
import Random
import Tuple


main : Program () () Never
main =
    Platform.worker
        { init =
            \_ ->
                ( ()
                , Terminal.writeLine
                    (String.fromInt
                        (Tuple.first (Random.step (Random.int 0 1000000) (Random.initialSeed 1)))
                    )
                )
        , update = \_ m -> ( m, Cmd.none )
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn random_step_is_deterministic_and_matches_js_native() {
    let dir = common::test_dir("alm-random", "step-differential");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), STEP_MAIN).unwrap();

    let checked = project::check_project(&src.join("Main.elm")).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });

    // JS backend under node.
    let js = generate::generate_project(&checked.modules);
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, &js).expect("write bundle");
    let js_output = Command::new("node")
        .arg("-e")
        .arg(format!("require({:?}).Elm['Main'].init({{}})", bundle.display()))
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .output()
        .expect("run node");
    assert!(
        js_output.status.success(),
        "node failed:\n{}",
        String::from_utf8_lossy(&js_output.stderr)
    );
    let js_out = String::from_utf8_lossy(&js_output.stdout).trim_end().to_string();

    // Native backend.
    let program = ir::lower::lower_project(&checked.modules);
    let binary = dir.join("prog");
    generate::native::build(&program, &binary, generate::native::OptLevel::Release)
        .unwrap_or_else(|e| panic!("native build failed: {e}"));
    let native_out = run_capped(&binary, Duration::from_secs(30));

    assert!(!js_out.is_empty(), "step produced no output");
    assert_eq!(js_out, native_out, "Random.step differs between JS and native");
}

// --- wasm-gc coverage -------------------------------------------------------
//
// The wasm-gc backend drives its port worker under Node. `host_now` is fixed at
// 0 so the manager's `Time.now`-derived seed is deterministic; the payload is
// JSON-encoded and written to stdout by `host_port_out`.
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

fn run_wasmgc(test_name: &str, main_src: &str) -> String {
    let dir = common::test_dir("alm-random", test_name);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), main_src).unwrap();

    let entry = src.join("Main.elm");
    let wasm = dir.join("app.wasm");
    project::compile_project_wasmgc(&entry, &wasm, false).unwrap_or_else(|e| {
        panic!("wasmgc build failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
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

// `Random.generate` through the manager (init seeds from `Time.now`, `onEffects`
// steps + `sendToApp`, update fires) → a die roll out a port.
const DIE_PORT_MAIN: &str = r#"port module Main exposing (main)

import Platform
import Random


port out : String -> Cmd msg


type Msg = Rolled Int


main : Program () () Msg
main =
    Platform.worker
        { init = \_ -> ( (), Random.generate Rolled (Random.int 1 6) )
        , update = \(Rolled n) m -> ( m, out (String.fromInt n) )
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn random_generate_reaches_app_wasmgc() {
    let out = run_wasmgc("generate-wasmgc", DIE_PORT_MAIN);
    let n: i64 = out.trim().parse().unwrap_or_else(|_| panic!("expected a die roll, got {out:?}"));
    assert!((1..=6).contains(&n), "Random.generate die roll {n} not in 1..6");
}

// `Random.step` is deterministic and must be byte-identical to the JS backend
// (same PCG algorithm + seed reps). A single value and a `Random.list` draw.
const STEP_PORT_MAIN: &str = r#"port module Main exposing (main)

import Platform
import Random
import Tuple


port out : String -> Cmd msg


detVal : Int
detVal =
    Tuple.first (Random.step (Random.int 0 1000000) (Random.initialSeed 1))


seq : List Int
seq =
    Tuple.first (Random.step (Random.list 5 (Random.int 0 1000000)) (Random.initialSeed 42))


main : Program () () ()
main =
    Platform.worker
        { init = \_ -> ( (), out (String.fromInt detVal ++ "|" ++ String.join "," (List.map String.fromInt seq)) )
        , update = \_ m -> ( m, Cmd.none )
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn random_step_deterministic_matches_js_wasmgc() {
    let dir = common::test_dir("alm-random", "step-wasmgc-js");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), STEP_PORT_MAIN).unwrap();

    // JS backend under node (subscribe to the port).
    let checked = project::check_project(&src.join("Main.elm")).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let js = generate::generate_project(&checked.modules);
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, &js).expect("write bundle");
    let js_output = Command::new("node")
        .arg("-e")
        .arg(format!(
            "const p=require({:?}).Elm;p['Main']['main'].init({{}}).ports.out.subscribe(v=>process.stdout.write(v));",
            bundle.display()
        ))
        .env_remove("FORCE_COLOR")
        .env("NO_COLOR", "1")
        .output()
        .expect("run node");
    assert!(js_output.status.success(), "node failed:\n{}", String::from_utf8_lossy(&js_output.stderr));
    let js_out = String::from_utf8_lossy(&js_output.stdout).trim_end().to_string();

    let wasm_out = run_wasmgc("step-wasmgc", STEP_PORT_MAIN);

    assert!(!wasm_out.is_empty(), "step produced no output");
    assert_eq!(js_out, wasm_out, "Random.step differs between JS and wasm-gc");
}
