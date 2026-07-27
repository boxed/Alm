//! `--optimize`: elm's production build.
//!
//! The optimizations it enables strip out what `Debug` needs — record field
//! names are shortened, single-constructor types unboxed — so elm refuses to
//! build while any `Debug` call survives, and names the modules to fix. That
//! refusal is a whole-build failure rather than a problem in some module: it
//! quotes no source, its header bar runs to the margin with no file after it,
//! and `--report=json` gives it elm's `{"type":"error"}` envelope instead of
//! `compile-errors`. Because the refusal happens before code generation, the
//! CLI never reaches the point of writing a bundle.

use alm_compiler::{debug_uses, project};

mod common;

/// A worker whose `init` body is `body`.
fn worker(body: &str) -> String {
    format!(
        "module Main exposing (main)\n\nimport Platform\n\n\nmain : Program () () ()\nmain =\n    \
         Platform.worker\n        {{ init = \\_ -> ( {body}, Cmd.none )\n        \
         , update = \\_ _ -> ( (), Cmd.none )\n        \
         , subscriptions = \\_ -> Sub.none\n        }}\n"
    )
}

fn write_project(source: &str) -> common::TestDir {
    let dir = common::test_dir("alm-optimize", "proj");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"], "elm-version": "0.19.1",
             "dependencies": { "direct": { "elm/core": "1.0.5" }, "indirect": {} },
             "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    std::fs::write(src.join("Main.elm"), source).unwrap();
    dir
}

#[test]
fn optimize_rejects_surviving_debug_calls() {
    for body in ["Debug.log \"hi\" ()", "Debug.todo \"x\""] {
        let dir = write_project(&worker(body));
        let result = project::compile_project_with(&dir.join("src/Main.elm"), true);
        let errors = match result {
            Ok(_) => panic!("--optimize should refuse to compile `{body}`"),
            Err(errors) => errors,
        };
        assert_eq!(errors.len(), 1, "one whole-build failure for `{body}`");
        assert!(errors[0].is_whole_build(), "`{body}` should not be blamed on a file");
        let rendered = errors[0].render();
        assert!(
            rendered.starts_with("-- DEBUG REMNANTS ---"),
            "unexpected report for `{body}`:\n{rendered}"
        );
        assert!(rendered.contains("\n    Main\n"), "should name the module:\n{rendered}");
        assert!(
            rendered.contains("But the --optimize flag only works if all `Debug` functions are"),
            "should explain the rule:\n{rendered}"
        );
        // The bar runs to the margin: no file name, so no trailing space.
        let bar = rendered.lines().next().unwrap();
        assert_eq!(bar.chars().count(), 80, "header bar should fill 80 columns: {bar:?}");
        assert!(!bar.ends_with(' '), "no path, so no trailing space: {bar:?}");
    }
}

/// The same program without `Debug` builds, and the bundle still runs.
#[test]
fn optimize_builds_a_working_bundle() {
    let dir = write_project(&worker("()"));
    let (javascript, _) = project::compile_project_with(&dir.join("src/Main.elm"), true)
        .unwrap_or_else(|errors| {
            panic!("{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
        });
    let path = common::write_js("optimize-worker", &javascript);
    let out = common::run_node(
        &format!(
            "var app = require({:?}).Elm.Main.init({{}});\
             console.log(typeof app === 'object');",
            path.display()
        ),
        &javascript,
    );
    assert_eq!(out, "true");
}

/// Only modules that really use `Debug` are named — the check walks the whole
/// expression tree, so a call buried in a `let` or a `case` branch counts, and
/// an unrelated module does not.
#[test]
fn only_modules_that_use_debug_are_named() {
    let dir = common::test_dir("alm-optimize", "which");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"], "elm-version": "0.19.1",
             "dependencies": { "direct": { "elm/core": "1.0.5" }, "indirect": {} },
             "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    std::fs::write(
        src.join("Clean.elm"),
        "module Clean exposing (value)\n\n\nvalue : Int\nvalue =\n    1\n",
    )
    .unwrap();
    std::fs::write(
        src.join("Dirty.elm"),
        "module Dirty exposing (value)\n\n\nvalue : Int\nvalue =\n    let\n        \
         step n =\n            case n of\n                0 ->\n                    \
         Debug.log \"zero\" 0\n\n                _ ->\n                    n\n    in\n    step 1\n",
    )
    .unwrap();
    std::fs::write(
        src.join("Main.elm"),
        "module Main exposing (main)\n\nimport Clean\nimport Dirty\nimport Platform\n\n\n\
         main : Program () () ()\nmain =\n    Platform.worker\n        \
         { init = \\_ -> ( always () (Clean.value + Dirty.value), Cmd.none )\n        \
         , update = \\_ _ -> ( (), Cmd.none )\n        \
         , subscriptions = \\_ -> Sub.none\n        }\n",
    )
    .unwrap();

    let checked = project::check_project(&src.join("Main.elm")).unwrap_or_else(|errors| {
        panic!("{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    let named: Vec<String> =
        debug_uses::modules_using_debug(&checked.modules).iter().map(|n| n.to_string()).collect();
    assert_eq!(named, vec!["Dirty".to_string()]);
}
