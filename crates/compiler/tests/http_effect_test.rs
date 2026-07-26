//! Http is a bundled effect module (not a builtin). Front-end check: a program
//! using `Http.get`/`expectString` type-checks through the bundled source (which
//! pulls elm/bytes from the package cache) and Http carries an effect-manager
//! declaration. Native check: the request flows through the manager without
//! crashing — native has no HTTP client, so the response is never delivered (I/O
//! unsupported), but the effect DISPATCH is a real manager.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use alm_compiler::{generate, ir, project};

// Only elm/core is declared: Http is bundled (not from the app's deps), and its
// elm/bytes dependency is resolved from the package cache unconditionally.
const ELM_JSON: &str = r#"{
    "type": "application",
    "source-directories": ["src"],
    "elm-version": "0.19.1",
    "dependencies": { "direct": { "elm/core": "1.0.5" }, "indirect": {} },
    "test-dependencies": { "direct": {}, "indirect": {} }
}"#;

const MAIN: &str = r#"module Main exposing (main)

import Http
import Platform


type alias Model = String
type Msg = Got (Result Http.Error String)


main : Program () Model Msg
main =
    Platform.worker
        { init = \_ -> ( "init", Http.get { url = "/data", expect = Http.expectString Got } )
        , update = \msg _ -> case msg of
            Got (Ok s) -> ( s, Cmd.none )
            Got (Err _) -> ( "err", Cmd.none )
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn http_program_typechecks_through_bundled_effect_module() {
    let dir = common::test_dir("alm-http", "check");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), MAIN).unwrap();

    match project::check_project(&src.join("Main.elm")) {
        Ok(checked) => {
            let http = checked
                .modules
                .iter()
                .find(|m| m.name.as_str() == "Http")
                .expect("Http module should be compiled from bundled source");
            assert!(http.manager.is_some(), "Http should be an effect module (have a manager)");
            // elm/bytes is resolved for the bundled Http module even though the
            // app only declares elm/core.
            assert!(
                checked.modules.iter().any(|m| m.name.as_str() == "Bytes.Decode"),
                "Bytes.Decode should be resolved for the bundled Http module"
            );
        }
        Err(errors) => {
            let msg = errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n");
            panic!("Http program failed to type-check:\n{msg}");
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
                    panic!("native Http program did not stop within {limit:?}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

// A worker that on init writes "started" and issues an `Http.get` through the
// effect-manager protocol (request -> command -> onEffects -> Process.spawn
// toTask). Native has no HTTP client, so the request never completes: the
// program must print only "started" (no response) and exit cleanly — proving the
// manager dispatch is wired and does not crash.
const NATIVE_MAIN: &str = r#"module Main exposing (main)

import Http
import Platform
import Terminal


type alias Model = Int
type Msg = Got (Result Http.Error String)


main : Program () Model Msg
main =
    Platform.worker
        { init =
            \_ ->
                ( 0
                , Cmd.batch
                    [ Terminal.writeLine "started"
                    , Http.get { url = "/data", expect = Http.expectString Got }
                    ]
                )
        , update = \_ _ -> ( 0, Terminal.writeLine "got-response" )
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn http_get_native_dispatches_through_manager_without_crash() {
    let dir = common::test_dir("alm-http", "get-native");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), NATIVE_MAIN).unwrap();

    let checked = project::check_project(&src.join("Main.elm")).unwrap_or_else(|e| {
        panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let program = ir::lower::lower_project(&checked.modules);
    let binary = dir.join("prog");
    generate::native::build(&program, &binary, generate::native::OptLevel::Release)
        .unwrap_or_else(|e| panic!("native build failed: {e}"));

    let out = run_capped(&binary, Duration::from_secs(30));
    // The request is dispatched but never completes natively (no HTTP client),
    // so only the init line is printed — and the program exits without crashing.
    assert_eq!(out, "started", "native Http.get should dispatch through the manager and not crash");
}
