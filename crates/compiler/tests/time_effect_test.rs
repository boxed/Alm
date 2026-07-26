//! Time is a real bundled effect module (not a builtin). These tests check that
//! a program using Time (calendar + `Time.every` subscription) type-checks
//! through the bundled source, and — once backends are wired — that `Time.every`
//! fires through the `_Platform` manager protocol.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use alm_compiler::{generate, ir, project};

const ELM_JSON: &str = r#"{
    "type": "application",
    "source-directories": ["src"],
    "elm-version": "0.19.1",
    "dependencies": {
        "direct": { "elm/core": "1.0.5", "elm/time": "1.0.0" },
        "indirect": {}
    },
    "test-dependencies": { "direct": {}, "indirect": {} }
}"#;

const MAIN: &str = r#"module Main exposing (main)

import Platform
import Task
import Time


type alias Model = { count : Int, last : Int }
type Msg = Tick Time.Posix


main : Program () Model Msg
main =
    Platform.worker
        { init = \_ -> ( { count = 0, last = 0 }, Cmd.none )
        , update =
            \msg model ->
                case msg of
                    Tick posix ->
                        ( { count = model.count + 1, last = Time.posixToMillis posix }, Cmd.none )
        , subscriptions = \_ -> Time.every 1000 Tick
        }
"#;

#[test]
fn time_program_typechecks_through_bundled_effect_module() {
    let dir = common::test_dir("alm-time", "check");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), MAIN).unwrap();

    match project::check_project(&src.join("Main.elm")) {
        Ok(checked) => {
            // Time must be present as a compiled module (the bundled effect module),
            // and it must carry an effect-manager declaration.
            let time = checked
                .modules
                .iter()
                .find(|m| m.name.as_str() == "Time")
                .expect("Time module should be compiled from bundled source");
            assert!(time.manager.is_some(), "Time should be an effect module (have a manager)");
        }
        Err(errors) => {
            let msg = errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n");
            panic!("Time program failed to type-check:\n{msg}");
        }
    }
}

// A worker that subscribes to `Time.every` while `count < 5`, printing the
// running count each tick, then returns `Sub.none` — which makes the Time
// manager `Process.kill` its `setInterval` timer, so the program stops on its
// own once five ticks have fired (a leaked timer would run forever).
const TICKING_MAIN: &str = r#"module Main exposing (main)

import Platform
import Time


type alias Model = Int
type Msg = Tick Time.Posix


main : Program () Model Msg
main =
    Platform.worker
        { init = \_ -> ( 0, Cmd.none )
        , update =
            \msg count ->
                case msg of
                    Tick _ ->
                        ( count + 1, Terminal.writeLine (String.fromInt (count + 1)) )
        , subscriptions =
            \count ->
                if count < 5 then
                    Time.every 40 Tick

                else
                    Sub.none
        }
"#;

/// Run `bin` with an out-of-band kill guard: `Time.every` timers could run
/// forever if cancellation were broken, and an uncapped native binary once
/// crashed the machine. Poll for exit up to `limit`; kill and fail otherwise.
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
                    panic!(
                        "native Time.every program did not stop within {:?} — \
                         a dropped subscription must Process.kill its timer",
                        limit
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn time_every_fires_then_stops_when_subscription_dropped_native() {
    let dir = common::test_dir("alm-time", "every-native");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), TICKING_MAIN).unwrap();

    let checked = project::check_project(&src.join("Main.elm"))
        .unwrap_or_else(|e| panic!("check failed:\n{}", e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")));

    let program = ir::lower::lower_project(&checked.modules);
    let binary = dir.join("prog");
    generate::native::build(&program, &binary, generate::native::OptLevel::Release)
        .unwrap_or_else(|e| panic!("native build failed: {e}"));

    // Five 40ms ticks plus process spawn overhead; 30s is a generous ceiling.
    let out = run_capped(&binary, Duration::from_secs(30));
    assert_eq!(out, "1\n2\n3\n4\n5", "Time.every should tick 5 times then stop");
}
