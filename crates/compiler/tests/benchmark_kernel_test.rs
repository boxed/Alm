//! elm-explorations/benchmark's timing kernel.
//!
//! `Elm.Kernel.Benchmark.operation`/`sample` had no implementation, so any
//! program that actually ran a benchmark hit an undefined reference. An
//! Operation is the thunk itself; `sample n op` runs it n times and reports
//! the elapsed milliseconds as a Task, mapping a blown stack to the package's
//! own `StackOverflow`.
//!
//! The package's Elm source is inlined (doc comments trimmed) so the test does
//! not depend on the real ELM_HOME — same approach as `bytes_test`.

use alm_compiler::{generate, project};
use std::process::Command;
use std::sync::Mutex;

mod common;

static ELM_HOME_LOCK: Mutex<()> = Mutex::new(());

/// elm-explorations/benchmark 1.0.2's `Benchmark/LowLevel.elm`, doc comments
/// trimmed; the parts that touch the kernel are verbatim.
const LOWLEVEL_ELM: &str = r#"module Benchmark.LowLevel exposing
    ( Operation, operation
    , sample, Error(..)
    )

import Elm.Kernel.Benchmark
import Task exposing (Task)


type Operation
    = Operation


operation : (() -> a) -> Operation
operation fn =
    Elm.Kernel.Benchmark.operation fn


type Error
    = StackOverflow
    | UnknownError String


sample : Int -> Operation -> Task Error Float
sample n operation_ =
    Elm.Kernel.Benchmark.sample n operation_
"#;

fn run(main: &str) -> String {
    let _guard = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = common::test_dir("alm-benchmark", "run");
    let src = dir.join("src");
    let pkg = dir.join("elm-home/0.19.1/packages/elm-explorations/benchmark/1.0.2");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(pkg.join("src/Benchmark")).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"], "elm-version": "0.19.1",
             "dependencies": { "direct": { "elm-explorations/benchmark": "1.0.2" }, "indirect": {} },
             "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("elm.json"),
        r#"{ "type": "package", "name": "elm-explorations/benchmark", "summary": "b",
             "license": "BSD-3-Clause", "version": "1.0.2", "exposed-modules": ["Benchmark.LowLevel"],
             "elm-version": "0.19.0 <= v < 0.20.0",
             "dependencies": { "elm/core": "1.0.0 <= v < 2.0.0" }, "test-dependencies": {} }"#,
    )
    .unwrap();
    std::fs::write(pkg.join("src/Benchmark/LowLevel.elm"), LOWLEVEL_ELM).unwrap();
    std::fs::write(src.join("Main.elm"), main).unwrap();

    std::env::set_var("ELM_HOME", dir.join("elm-home"));
    let checked = project::check_project(&src.join("Main.elm"));
    std::env::remove_var("ELM_HOME");
    let checked = checked.unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });

    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).unwrap();
    let out = Command::new("node")
        .arg("-e")
        .arg(format!(
            "var app = require({:?}).Elm.Main.init({{}});\
             app.ports.out.subscribe(function (v) {{ process.stdout.write(v); process.exit(0); }});\
             setTimeout(function () {{ process.stdout.write('TIMEOUT'); process.exit(1); }}, 20000);",
            bundle.display()
        ))
        .env_remove("FORCE_COLOR")
        .output()
        .expect("run node");
    assert!(out.status.success(), "node failed:\n{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

const PROGRAM: &str = r#"port module Main exposing (main)

import Benchmark.LowLevel as BL
import Platform
import Task


port out : String -> Cmd msg


type Msg
    = Done (Result BL.Error Float)


main : Program () () Msg
main =
    Platform.worker
        { init = \_ -> ( (), Task.attempt Done (BL.sample 100 (BL.operation (\() -> WORK))) )
        , update =
            \msg _ ->
                case msg of
                    Done (Ok elapsed) ->
                        ( ()
                        , out
                            (if elapsed >= 0 then
                                "ok"

                             else
                                "negative elapsed"
                            )
                        )

                    Done (Err BL.StackOverflow) ->
                        ( (), out "StackOverflow" )

                    Done (Err (BL.UnknownError e)) ->
                        ( (), out ("UnknownError " ++ e) )
        , subscriptions = \_ -> Sub.none
        }
"#;

/// The happy path: the thunk runs, the task succeeds with an elapsed time.
#[test]
fn sample_times_the_operation() {
    assert_eq!(run(&PROGRAM.replace("WORK", "List.sum (List.range 1 100)")), "ok");
}

/// A thunk that blows the stack fails the task as `StackOverflow` rather than
/// escaping as a JS exception.
#[test]
fn a_blown_stack_becomes_stack_overflow() {
    // Non-tail recursion deep enough to exhaust the JS stack.
    let work = "deep 1e9";
    let program = PROGRAM.replace("WORK", work).replace(
        "port out : String -> Cmd msg",
        "port out : String -> Cmd msg\n\n\ndeep : Float -> Float\ndeep n =\n    if n <= 0 then\n        0\n\n    else\n        1 + deep (n - 1)",
    );
    assert_eq!(run(&program), "StackOverflow");
}
