//! End-to-end tests for the wasm32-wasi backend: compile a fixture to a
//! `.wasm` module and run it with node's WASI, comparing stdout with what
//! the JS backend prints. Requires the wasm32-wasi target and node.

use std::process::Command;

use alm_compiler::{generate, ir, project};

fn run_both(test_name: &str, source: &str) -> (String, String) {
    let dir = std::env::temp_dir()
        .join("alm-wasm-tests")
        .join(format!("{}-{}", test_name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");

    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!(
            "check failed:\n{}",
            errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
        )
    });

    // JS backend, via node.
    let js = generate::generate_project(&checked.modules);
    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, js).expect("write bundle");
    let js_out = run(Command::new("node").arg("-e").arg(format!(
        "console.log(require({:?})['Test']['main'])",
        bundle.display()
    )));

    // Wasm backend, via node's WASI.
    let program = ir::lower::lower_project(&checked.modules);
    let wasm = dir.join("test.wasm");
    generate::native::build(&program, &wasm, generate::native::Target::Wasm)
        .unwrap_or_else(|e| panic!("wasm build failed: {}", e));
    let runner = dir.join("run.cjs");
    std::fs::write(
        &runner,
        format!(
            "const {{WASI}}=require('node:wasi');const fs=require('fs');(async()=>{{\
             const wasi=new WASI({{version:'preview1',args:['p'],env:{{}},returnOnExit:true}});\
             const m=await WebAssembly.compile(fs.readFileSync({:?}));\
             const i=await WebAssembly.instantiate(m,wasi.getImportObject());\
             wasi.start(i);}})();",
            wasm.display()
        ),
    )
    .expect("write runner");
    let wasm_out = run(Command::new("node").arg("--no-warnings").arg(&runner));

    (js_out, wasm_out)
}

fn run(command: &mut Command) -> String {
    let output = command.output().expect("spawn node");
    assert!(
        output.status.success(),
        "failed with {:?}:\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim_end().to_string()
}

/// Compile a `Platform.worker` program to wasm and run it under node's
/// WASI, returning what it printed (the TEA event loop drives sleep/timers
/// through WASI's poll_oneoff/clock_time_get).
fn run_worker_wasm(test_name: &str, source: &str) -> String {
    let dir = std::env::temp_dir()
        .join("alm-wasm-tea")
        .join(format!("{}-{}", test_name, std::process::id()));
    std::fs::create_dir_all(&dir).expect("create test dir");
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let program = ir::lower::lower_project(&checked.modules);
    let wasm = dir.join("test.wasm");
    generate::native::build(&program, &wasm, generate::native::Target::Wasm)
        .unwrap_or_else(|e| panic!("wasm build failed: {}", e));
    let runner = dir.join("run.cjs");
    std::fs::write(
        &runner,
        format!(
            "const {{WASI}}=require('node:wasi');const fs=require('fs');(async()=>{{\
             const wasi=new WASI({{version:'preview1',args:['p'],env:{{}},returnOnExit:true}});\
             const m=await WebAssembly.compile(fs.readFileSync({:?}));\
             const i=await WebAssembly.instantiate(m,wasi.getImportObject());\
             wasi.start(i);}})();",
            wasm.display()
        ),
    )
    .expect("write runner");
    run(Command::new("node").arg("--no-warnings").arg(&runner))
}

#[test]
fn tea_worker_and_timers() {
    // The TEA event loop, timer subscriptions and Process.sleep all run
    // under WASI (sleep/clock via poll_oneoff/clock_time_get).
    let ticks = run_worker_wasm(
        "ticks",
        "module Test exposing (..)\n\
         \n\
         import Time\n\
         \n\
         type Msg = Tick Time.Posix\n\
         \n\
         main =\n\
         \x20   Platform.worker { init = \\_ -> ( 0, Cmd.none ), update = update, subscriptions = subs }\n\
         \n\
         update msg model =\n\
         \x20   case msg of\n\
         \x20       Tick _ -> ( model + 1, Terminal.writeLine (\"tick \" ++ String.fromInt (model + 1)) )\n\
         \n\
         subs model =\n\
         \x20   if model < 3 then Time.every 10 Tick else Sub.none\n",
    );
    assert_eq!(ticks, "tick 1\ntick 2\ntick 3");
}

fn assert_same(test_name: &str, source: &str) {
    let (js, wasm) = run_both(test_name, source);
    assert!(!js.is_empty(), "JS output is empty");
    assert_eq!(wasm, js, "wasm and JS backends disagree");
}

#[test]
fn strings_and_lists() {
    assert_same(
        "strings_lists",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   List.range 1 10\n\
         \x20       |> List.filter (\\n -> modBy 2 n == 0)\n\
         \x20       |> List.map (\\n -> n * n)\n\
         \x20       |> List.map String.fromInt\n\
         \x20       |> String.join \", \"\n",
    );
}

#[test]
fn large_integers() {
    // 64-bit ints must survive on wasm32 (boxed, not truncated into a
    // 32-bit pointer) — this is the regression the value representation fixes.
    assert_same(
        "large_ints",
        "module Test exposing (..)\n\
         \n\
         main =\n\
         \x20   String.fromInt (1000000 * 1000000)\n\
         \x20       ++ \" \"\n\
         \x20       ++ String.fromInt (List.sum (List.range 1 100000))\n",
    );
}

#[test]
fn custom_types_and_debug() {
    assert_same(
        "custom_debug",
        "module Test exposing (..)\n\
         \n\
         type Tree\n\
         \x20   = Leaf Int\n\
         \x20   | Node Tree Tree\n\
         \n\
         sum tree =\n\
         \x20   case tree of\n\
         \x20       Leaf n -> n\n\
         \x20       Node l r -> sum l + sum r\n\
         \n\
         main =\n\
         \x20   let\n\
         \x20       t = Node (Node (Leaf 1) (Leaf 2)) (Leaf 3)\n\
         \x20   in\n\
         \x20   Debug.toString ( sum t, Just t )\n",
    );
}
