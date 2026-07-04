//! The Elm Architecture on the native backend: Platform.worker programs
//! driven by the C event loop, checked against the JS runtime under node
//! (both print via Terminal.writeLine and exit when nothing is pending).

use std::process::Command;

use alm_compiler::{generate, ir, project};

fn run_both(test_name: &str, source: &str) -> (String, String) {
    let dir = std::env::temp_dir()
        .join("alm-native-tea")
        .join(format!("{}-{}", test_name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");

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

    let js = generate::generate_project(&checked.modules);
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, js).expect("write bundle");
    let js_out = run_command(
        Command::new("node").arg("-e").arg(format!(
            "require({:?})['Test']['main'].init({{}})",
            bundle.display()
        )),
        "node",
    );

    let program = ir::lower::lower_project(&checked.modules);
    let binary = dir.join("test");
    generate::native::build(&program, &binary, generate::native::Target::Native)
        .unwrap_or_else(|e| panic!("native build failed: {}", e));
    let native_out = run_command(&mut Command::new(&binary), "native binary");

    (js_out, native_out)
}

fn run_command(command: &mut Command, what: &str) -> String {
    let output = command.output().unwrap_or_else(|e| panic!("run {}: {}", what, e));
    assert!(
        output.status.success(),
        "{} failed with {:?}:\nstdout: {}\nstderr: {}",
        what,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

fn assert_program_output(test_name: &str, source: &str, expected: &str) {
    let (js, native) = run_both(test_name, source);
    assert_eq!(js, expected, "JS backend output");
    assert_eq!(native, expected, "native backend output");
}

#[test]
fn timer_subscription_ticks_then_exits() {
    assert_program_output(
        "ticks",
        "module Test exposing (..)\n\
         \n\
         import Time\n\
         \n\
         type Msg\n\
         \x20   = Tick Time.Posix\n\
         \n\
         main =\n\
         \x20   Platform.worker\n\
         \x20       { init = \\_ -> ( 0, Cmd.none )\n\
         \x20       , update = update\n\
         \x20       , subscriptions = subscriptions\n\
         \x20       }\n\
         \n\
         update msg model =\n\
         \x20   case msg of\n\
         \x20       Tick _ ->\n\
         \x20           ( model + 1\n\
         \x20           , Terminal.writeLine (\"tick \" ++ String.fromInt (model + 1))\n\
         \x20           )\n\
         \n\
         subscriptions model =\n\
         \x20   if model < 3 then\n\
         \x20       Time.every 10 Tick\n\
         \x20   else\n\
         \x20       Sub.none\n",
        "tick 1\ntick 2\ntick 3",
    );
}

#[test]
fn task_chain_delivers_message() {
    assert_program_output(
        "task_chain",
        "module Test exposing (..)\n\
         \n\
         import Task\n\
         \n\
         type Msg\n\
         \x20   = Done Int\n\
         \n\
         main =\n\
         \x20   Platform.worker\n\
         \x20       { init = init, update = update, subscriptions = \\_ -> Sub.none }\n\
         \n\
         init _ =\n\
         \x20   ( 0\n\
         \x20   , Task.succeed 20\n\
         \x20       |> Task.andThen (\\n -> Task.succeed (n + 1))\n\
         \x20       |> Task.map (\\n -> n * 2)\n\
         \x20       |> Task.perform Done\n\
         \x20   )\n\
         \n\
         update msg model =\n\
         \x20   case msg of\n\
         \x20       Done n ->\n\
         \x20           ( model, Terminal.writeLine (String.fromInt n) )\n",
        "42",
    );
}

#[test]
fn process_sleep_orders_by_duration() {
    assert_program_output(
        "sleep_order",
        "module Test exposing (..)\n\
         \n\
         import Process\n\
         import Task\n\
         \n\
         type Msg\n\
         \x20   = Slept String\n\
         \n\
         main =\n\
         \x20   Platform.worker\n\
         \x20       { init = init, update = update, subscriptions = \\_ -> Sub.none }\n\
         \n\
         init _ =\n\
         \x20   ( 0\n\
         \x20   , Cmd.batch\n\
         \x20       [ Task.perform (\\_ -> Slept \"slow\") (Process.sleep 60)\n\
         \x20       , Task.perform (\\_ -> Slept \"fast\") (Process.sleep 15)\n\
         \x20       , Terminal.writeLine \"start\"\n\
         \x20       ]\n\
         \x20   )\n\
         \n\
         update msg model =\n\
         \x20   case msg of\n\
         \x20       Slept label ->\n\
         \x20           ( model, Terminal.writeLine label )\n",
        "start\nfast\nslow",
    );
}

#[test]
fn task_failure_and_recovery() {
    assert_program_output(
        "task_errors",
        "module Test exposing (..)\n\
         \n\
         import Task\n\
         \n\
         type Msg\n\
         \x20   = Got (Result String Int)\n\
         \x20   | Recovered String\n\
         \n\
         main =\n\
         \x20   Platform.worker\n\
         \x20       { init = init, update = update, subscriptions = \\_ -> Sub.none }\n\
         \n\
         init _ =\n\
         \x20   ( 0\n\
         \x20   , Cmd.batch\n\
         \x20       [ Task.attempt Got (Task.fail \"boom\")\n\
         \x20       , Task.fail \"bang\"\n\
         \x20           |> Task.onError (\\e -> Task.succeed (\"recovered \" ++ e))\n\
         \x20           |> Task.perform Recovered\n\
         \x20       ]\n\
         \x20   )\n\
         \n\
         update msg model =\n\
         \x20   case msg of\n\
         \x20       Got result ->\n\
         \x20           ( model, Terminal.writeLine (Debug.toString result) )\n\
         \n\
         \x20       Recovered text ->\n\
         \x20           ( model, Terminal.writeLine text )\n",
        "Err \"boom\"\nrecovered bang",
    );
}

#[test]
fn task_sequence_and_map2() {
    assert_program_output(
        "task_sequence",
        "module Test exposing (..)\n\
         \n\
         import Task\n\
         \n\
         type Msg\n\
         \x20   = Done (List Int)\n\
         \x20   | Sum Int\n\
         \n\
         main =\n\
         \x20   Platform.worker\n\
         \x20       { init = init, update = update, subscriptions = \\_ -> Sub.none }\n\
         \n\
         init _ =\n\
         \x20   ( 0\n\
         \x20   , Cmd.batch\n\
         \x20       [ Task.perform Done (Task.sequence [ Task.succeed 1, Task.succeed 2, Task.succeed 3 ])\n\
         \x20       , Task.perform Sum (Task.map2 (\\a b -> a + b) (Task.succeed 40) (Task.succeed 2))\n\
         \x20       ]\n\
         \x20   )\n\
         \n\
         update msg model =\n\
         \x20   case msg of\n\
         \x20       Done xs ->\n\
         \x20           ( model, Terminal.writeLine (Debug.toString xs) )\n\
         \n\
         \x20       Sum n ->\n\
         \x20           ( model, Terminal.writeLine (String.fromInt n) )\n",
        "[1,2,3]\n42",
    );
}
