//! The incremental build cache has exactly one contract: whatever it does, the
//! bundle must be what a full build would have produced. A cache that is only
//! nearly right is worse than no cache at all, because the difference surfaces
//! as a bug in the user's program, days later, in code they did not touch.
//!
//! So every test here is differential — edit something, build incrementally,
//! build again from scratch, compare — rather than an assertion about which
//! modules the cache decided to reuse. The one exception is
//! `dependent_is_rechecked_when_an_interface_changes`, which pins the property
//! the whole design rests on: a module whose own text is untouched still has to
//! be re-checked when something it imports changes shape. Get that wrong and
//! the build happily emits code against types that no longer exist.

mod common;

use std::path::Path;

use alm_compiler::project;

const MAIN: &str = r#"module Main exposing (main)

import Platform
import Widget


main : Program () () ()
main =
    Platform.worker
        { init = \_ -> ( (), Cmd.none )
        , update = \_ model -> ( model, Cmd.none )
        , subscriptions = \_ -> Sub.none
        }


described : String
described =
    Widget.describe Widget.sample
"#;

const WIDGET: &str = r#"module Widget exposing (Widget, describe, sample)

import Label


type alias Widget =
    { name : String, size : Int }


sample : Widget
sample =
    { name = Label.title, size = 3 }


describe : Widget -> String
describe w =
    Label.render w.name ++ " (" ++ String.fromInt w.size ++ ")"
"#;

const LABEL: &str = r#"module Label exposing (render, title)


title : String
title =
    "widget"


render : String -> String
render text =
    String.toUpper text
"#;

/// A three-module chain — Main imports Widget imports Label — so a change to
/// `Label` has to travel two levels to reach `Main`.
fn project_with(dir: &Path, label: &str) {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("create src");
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"], "elm-version": "0.19.1",
            "dependencies": { "direct": { "elm/core": "1.0.5" }, "indirect": {} },
            "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .expect("write elm.json");
    std::fs::write(src.join("Main.elm"), MAIN).expect("write Main");
    std::fs::write(src.join("Widget.elm"), WIDGET).expect("write Widget");
    std::fs::write(src.join("Label.elm"), label).expect("write Label");
}

fn build_incremental(dir: &Path) -> Result<String, String> {
    project::compile_project_cached(&dir.join("src").join("Main.elm"), false, true)
        .map(|(js, _)| js)
        .map_err(render)
}

fn build_full(dir: &Path) -> Result<String, String> {
    project::compile_project_uncached(&dir.join("src").join("Main.elm"), false)
        .map(|(js, _)| js)
        .map_err(render)
}

fn render(errors: Vec<project::BuildError>) -> String {
    errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
}

/// Build incrementally and from scratch, and require the two to be identical.
fn assert_agrees(dir: &Path, what: &str) -> String {
    let incremental = build_incremental(dir);
    let full = build_full(dir);
    match (incremental, full) {
        (Ok(a), Ok(b)) => {
            assert_eq!(
                a.len(),
                b.len(),
                "{what}: incremental bundle is {} bytes, full build is {}",
                a.len(),
                b.len()
            );
            assert!(a == b, "{what}: incremental and full bundles differ");
            a
        }
        (Err(a), Err(b)) => {
            assert_eq!(a, b, "{what}: the two paths reported different errors");
            a
        }
        (Ok(_), Err(b)) => panic!("{what}: incremental succeeded but a full build failed:\n{b}"),
        (Err(a), Ok(_)) => panic!("{what}: incremental failed but a full build succeeded:\n{a}"),
    }
}

#[test]
fn cold_warm_and_full_builds_agree() {
    let dir = common::test_dir("alm-incremental", "agree");
    project_with(&dir, LABEL);

    let cold = assert_agrees(&dir, "cold");
    // Warm: everything now comes out of the cache. Same bytes, still.
    let warm = build_incremental(&dir).expect("warm build");
    assert_eq!(cold, warm, "a second build with a full cache changed the bundle");
}

#[test]
fn body_change_does_not_disturb_the_bundle() {
    let dir = common::test_dir("alm-incremental", "body");
    project_with(&dir, LABEL);
    build_incremental(&dir).expect("seed the cache");

    // `render`'s implementation changes; its type does not. Dependents may be
    // reused, but the bundle must still pick up the new body.
    let edited = LABEL.replace("String.toUpper text", "String.toLower text");
    std::fs::write(dir.join("src").join("Label.elm"), &edited).expect("edit Label");
    let js = assert_agrees(&dir, "after a body-only change");
    assert!(js.contains("toLower"), "the edited body never reached the bundle");
}

#[test]
fn dependent_is_rechecked_when_an_interface_changes() {
    let dir = common::test_dir("alm-incremental", "interface");
    project_with(&dir, LABEL);
    build_incremental(&dir).expect("seed the cache");

    // `title` becomes an Int. `Widget.sample` uses it as a String and its own
    // text is untouched, so if the cache reuses Widget on the strength of that,
    // the build "succeeds" and emits a program built against a type that is no
    // longer there. It must fail instead — with the same error a full build gives.
    let edited = LABEL.replace("title : String", "title : Int").replace("    \"widget\"", "    42");
    std::fs::write(dir.join("src").join("Label.elm"), &edited).expect("edit Label");

    let incremental = build_incremental(&dir);
    let full = build_full(&dir);
    assert!(
        incremental.is_err(),
        "the cache reused a dependent whose dependency changed shape — this is the \
         stale-build bug the design exists to prevent"
    );
    assert_eq!(
        incremental.unwrap_err(),
        full.unwrap_err(),
        "incremental and full builds disagreed about the error"
    );
}

#[test]
fn reverting_an_edit_restores_the_original_bundle() {
    let dir = common::test_dir("alm-incremental", "revert");
    project_with(&dir, LABEL);
    let original = build_incremental(&dir).expect("seed the cache");

    let edited = LABEL.replace("String.toUpper text", "String.toLower text");
    std::fs::write(dir.join("src").join("Label.elm"), &edited).expect("edit Label");
    build_incremental(&dir).expect("build the edit");

    std::fs::write(dir.join("src").join("Label.elm"), LABEL).expect("revert Label");
    assert_eq!(
        build_incremental(&dir).expect("build the revert"),
        original,
        "reverting an edit did not restore the original bundle"
    );
}

#[test]
fn a_new_module_in_the_middle_is_picked_up() {
    let dir = common::test_dir("alm-incremental", "new-module");
    project_with(&dir, LABEL);
    build_incremental(&dir).expect("seed the cache");

    // Widget starts importing a module that did not exist during the last
    // build. Nothing in the cache knows about it.
    std::fs::write(
        dir.join("src").join("Suffix.elm"),
        "module Suffix exposing (mark)\n\n\nmark : String\nmark =\n    \"!\"\n",
    )
    .expect("write Suffix");
    let edited = WIDGET
        .replace("import Label", "import Label\nimport Suffix")
        .replace("Label.render w.name ++", "Label.render w.name ++ Suffix.mark ++");
    std::fs::write(dir.join("src").join("Widget.elm"), &edited).expect("edit Widget");

    let js = assert_agrees(&dir, "after adding a module");
    assert!(js.contains("Suffix"), "the new module never reached the bundle");
}

#[test]
fn a_corrupt_entry_is_a_miss_not_a_failure() {
    let dir = common::test_dir("alm-incremental", "corrupt");
    project_with(&dir, LABEL);
    let original = build_incremental(&dir).expect("seed the cache");

    // Truncated files, foreign files, garbage: a cache is not a database, and
    // none of this may take a build down. Everything under `.alm-stuff` —
    // module entries, the module graph, the type-checker output — is fair game.
    let cache_dir = alm_compiler::cache::dir_for(&dir);
    build_wasm(&dir, true).expect("seed the wasm cache too");
    fn truncate_everything(at: &Path) {
        for entry in std::fs::read_dir(at).expect("read cache dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                truncate_everything(&path);
                continue;
            }
            let bytes = std::fs::read(&path).expect("read entry");
            std::fs::write(&path, &bytes[..bytes.len() / 2]).expect("truncate entry");
        }
    }
    truncate_everything(&cache_dir);
    std::fs::write(cache_dir.join("junk.almc"), b"not a cache entry at all").expect("write junk");

    assert_eq!(
        build_incremental(&dir).expect("build with a corrupt cache"),
        original,
        "a corrupt cache changed the output instead of being ignored"
    );
    assert_wasm_agrees(&dir, "wasm with a corrupt cache");
}

// The graph cache recognizes an untouched file by its timestamp and length, so
// that an unchanged module is never read or parsed. These pin the two ways that
// could go wrong.

#[test]
fn a_same_length_edit_is_still_noticed() {
    let dir = common::test_dir("alm-incremental", "same-length");
    project_with(&dir, LABEL);
    build_incremental(&dir).expect("seed the cache");

    // Same number of bytes, different bytes: only the timestamp gives it away.
    let edited = LABEL.replace("String.toUpper text", "String.toLower text");
    assert_eq!(edited.len(), LABEL.len(), "the fixture edit changed the length");
    std::fs::write(dir.join("src").join("Label.elm"), &edited).expect("edit Label");

    let js = assert_agrees(&dir, "after a same-length edit");
    assert!(js.contains("toLower"), "a same-length edit was missed");
}

#[test]
fn a_deleted_module_is_reported_not_silently_reused() {
    let dir = common::test_dir("alm-incremental", "deleted");
    project_with(&dir, LABEL);
    build_incremental(&dir).expect("seed the cache");

    // Widget still imports Label and its own text is untouched, so its cached
    // edge points at a file that is gone. That has to be an error, not a build
    // from the copy the cache happens to be holding.
    std::fs::remove_file(dir.join("src").join("Label.elm")).expect("delete Label");
    let error = assert_agrees(&dir, "after deleting an imported module");
    assert!(
        error.contains("Label"),
        "the error does not mention the missing module:\n{error}"
    );
}

#[test]
fn moving_a_module_between_source_dirs_is_picked_up() {
    let dir = common::test_dir("alm-incremental", "moved");
    project_with(&dir, LABEL);
    // A second source directory, searched after `src`.
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src", "vendor"],
            "elm-version": "0.19.1",
            "dependencies": { "direct": { "elm/core": "1.0.5" }, "indirect": {} },
            "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .expect("write elm.json");
    let original = build_incremental(&dir).expect("seed the cache");

    // Same module name, different file, different contents: the importer's text
    // has not changed, so only re-resolution can find it.
    std::fs::create_dir_all(dir.join("vendor")).expect("create vendor");
    std::fs::remove_file(dir.join("src").join("Label.elm")).expect("remove from src");
    std::fs::write(
        dir.join("vendor").join("Label.elm"),
        LABEL.replace("String.toUpper text", "String.trim text"),
    )
    .expect("write vendor Label");

    let js = assert_agrees(&dir, "after moving a module to another source dir");
    assert_ne!(js, original, "moving the module changed nothing in the bundle");
    assert!(js.contains("trim"), "the moved module's new body never arrived");
}

// wasm-gc and native cannot reuse a module wholesale — monomorphization is
// whole-program — so they cache the type checker's output instead and rebuild
// the AST from source every time. The contract is the same: byte-for-byte what
// a full build produces.

fn build_wasm(dir: &Path, use_cache: bool) -> Result<Vec<u8>, String> {
    let out = dir.join(if use_cache { "incr.wasm" } else { "full.wasm" });
    project::compile_project_wasmgc_with(
        &dir.join("src").join("Main.elm"),
        &out,
        false,
        use_cache,
    )
    .map_err(render)?;
    std::fs::read(&out).map_err(|e| e.to_string())
}

fn assert_wasm_agrees(dir: &Path, what: &str) -> Vec<u8> {
    let incremental = build_wasm(dir, true);
    let full = build_wasm(dir, false);
    match (incremental, full) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a.len(), b.len(), "{what}: wasm is {} bytes against {}", a.len(), b.len());
            assert!(a == b, "{what}: incremental and full wasm differ");
            a
        }
        (Err(a), Err(b)) => {
            assert_eq!(a, b, "{what}: the two paths reported different errors");
            Vec::new()
        }
        (Ok(_), Err(b)) => panic!("{what}: incremental succeeded but a full build failed:\n{b}"),
        (Err(a), Ok(_)) => panic!("{what}: incremental failed but a full build succeeded:\n{a}"),
    }
}

#[test]
fn wasm_cold_warm_and_full_builds_agree() {
    let dir = common::test_dir("alm-incremental", "wasm-agree");
    project_with(&dir, LABEL);

    let cold = assert_wasm_agrees(&dir, "wasm cold");
    let warm = build_wasm(&dir, true).expect("warm wasm build");
    assert_eq!(cold, warm, "a second wasm build with a full cache changed the output");
}

#[test]
fn wasm_picks_up_a_body_change() {
    let dir = common::test_dir("alm-incremental", "wasm-body");
    project_with(&dir, LABEL);
    build_wasm(&dir, true).expect("seed the cache");

    let edited = LABEL.replace("String.toUpper text", "String.toLower text");
    std::fs::write(dir.join("src").join("Label.elm"), &edited).expect("edit Label");
    let before = build_wasm(&dir, false).expect("reference build");
    let after = assert_wasm_agrees(&dir, "wasm after a body change");
    assert_eq!(after, before, "the edited body never reached the wasm output");
}

#[test]
fn wasm_dependent_is_rechecked_when_an_interface_changes() {
    let dir = common::test_dir("alm-incremental", "wasm-interface");
    project_with(&dir, LABEL);
    build_wasm(&dir, true).expect("seed the cache");

    // The same trap as the JavaScript path: `Widget`'s own text is untouched
    // but the type it depends on changed, and reusing its cached types would
    // hand monomorphization a program that no longer type checks.
    let edited = LABEL.replace("title : String", "title : Int").replace("    \"widget\"", "    42");
    std::fs::write(dir.join("src").join("Label.elm"), &edited).expect("edit Label");

    assert!(
        build_wasm(&dir, true).is_err(),
        "the cache reused type-checker output for a module whose dependency changed shape"
    );
}

#[test]
fn a_wasm_build_does_not_disturb_the_javascript_cache() {
    let dir = common::test_dir("alm-incremental", "both-targets");
    project_with(&dir, LABEL);

    // The two caches are keyed by module path, so without separate homes each
    // target would evict the other's entries and neither would ever hit.
    let js = build_incremental(&dir).expect("seed the js cache");
    build_wasm(&dir, true).expect("seed the wasm cache");
    assert_eq!(build_incremental(&dir).expect("js again"), js, "the wasm build changed the js output");
    assert_eq!(
        build_incremental(&dir).expect("js once more"),
        build_full(&dir).expect("js full"),
        "the js cache stopped agreeing with a full build after a wasm build"
    );
}
