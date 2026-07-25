//! User-defined `effect module`s run through alm's effect-manager protocol.
//! Stock elm restricts effect modules to the @elm organization; alm allows
//! them. Each program is a `Platform.worker` that observes the manager's
//! callback through `Terminal.writeLine`. Each program is compiled and run on
//! both the JS and native backends, and the two outputs must agree.

mod common;

use std::process::Command;

use alm_compiler::{generate, ir, project};

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

/// command -> onEffects -> sendToApp -> update.
#[test]
fn command_reaches_the_app() {
    let toast = r#"effect module Toast where { command = MyCmd } exposing (say)

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
        run("command", &[("Toast.elm", toast), ("Main.elm", main)]),
        "command-ok"
    );
}

/// command -> onEffects -> sendToSelf -> onSelfMsg -> sendToApp -> update.
#[test]
fn self_message_round_trip() {
    let toast = r#"effect module Toast where { command = MyCmd } exposing (echo)

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
        run("self", &[("Toast.elm", toast), ("Main.elm", main)]),
        "self-ok"
    );
}

/// subscription -> onEffects (with the current sub list) -> sendToApp. The
/// manager fires once and remembers it, so the app does not loop.
#[test]
fn subscription_reaches_the_app() {
    let ticker = r#"effect module Ticker where { subscription = MySub } exposing (listen)

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
        run("subscription", &[("Ticker.elm", ticker), ("Main.elm", main)]),
        "sub-ok"
    );
}
