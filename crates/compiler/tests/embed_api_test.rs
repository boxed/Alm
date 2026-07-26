//! The compiled bundle must present the same embedding API as `elm make`.
//!
//! alm used to export its `Elm` object *as* the CommonJS module and hang every
//! top-level binding off each module, so a program was booted with
//! `require(bundle).Main.main.init(...)` where elm wants
//! `require(bundle).Elm.Main.init(...)` — the embed snippet from any Elm guide
//! or README did not work. The extra bindings are still exported (they are a
//! superset elm does not mind), but `init` now sits where elm puts it.

mod common;

const WORKER: &str = r#"module Main exposing (main)

import Platform


main : Program () () ()
main =
    Platform.worker
        { init = \_ -> ( (), Cmd.none )
        , update = \_ _ -> ( (), Cmd.none )
        , subscriptions = \_ -> Sub.none
        }
"#;

fn probe(source: &str, expression: &str) -> String {
    let javascript = common::compile_single("Main.elm", source);
    let path = common::write_js("embed-api", &javascript);
    common::run_node(
        &format!("var bundle = require({:?});\nconsole.log({expression});", path.display()),
        &javascript,
    )
}

#[test]
fn commonjs_export_is_dot_elm_like_elm_make() {
    assert_eq!(probe(WORKER, "Object.keys(bundle).join(',')"), "Elm");
    assert_eq!(probe(WORKER, "typeof bundle.Elm.Main.init"), "function");
}

/// The bundle publishes itself into whatever scope loaded it, so dropping it
/// into a browser (where `this` is the global object) yields `window.Elm`.
#[test]
fn browser_global_export_is_elm() {
    let javascript = common::compile_single("Main.elm", WORKER);
    let path = common::write_js("embed-api-global", &javascript);
    let out = common::run_node(
        &format!(
            "var src = require('fs').readFileSync({:?}, 'utf8');\
             var window = {{}};\
             (new Function(src)).call(window);\
             console.log(typeof window.Elm.Main.init);",
            path.display()
        ),
        &javascript,
    );
    assert_eq!(out, "function");
}

/// alm exports every top-level binding, so a program could previously be
/// shadowed by an ordinary value named `init`. The program's initializer is
/// applied last and wins.
#[test]
fn a_binding_named_init_does_not_shadow_the_program() {
    let source = r#"module Main exposing (init, main)

import Platform


init : String
init =
    "not the program"


main : Program () () ()
main =
    Platform.worker
        { init = \_ -> ( (), Cmd.none )
        , update = \_ _ -> ( (), Cmd.none )
        , subscriptions = \_ -> Sub.none
        }
"#;
    assert_eq!(probe(source, "typeof bundle.Elm.Main.init"), "function");
}

/// Two bundles sharing one scope merge rather than clobbering each other.
#[test]
fn a_second_bundle_merges_into_the_same_elm_object() {
    let first = common::compile_single("Main.elm", WORKER);
    let second = common::compile_single(
        "Other.elm",
        &WORKER.replace("module Main ", "module Other "),
    );
    let a = common::write_js("embed-api-merge-a", &first);
    let b = common::write_js("embed-api-merge-b", &second);
    let out = common::run_node(
        &format!(
            "var fs = require('fs');\
             var scope = {{}};\
             (new Function(fs.readFileSync({:?}, 'utf8'))).call(scope);\
             (new Function(fs.readFileSync({:?}, 'utf8'))).call(scope);\
             console.log(Object.keys(scope.Elm).sort().join(','));",
            a.display(),
            b.display()
        ),
        &first,
    );
    assert_eq!(out, "Main,Other");
}
