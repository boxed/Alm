//! elm-explorations/markdown renders through a real parser.
//!
//! `Elm.Kernel.Markdown.toHtml` had no implementation, so a bundle that so
//! much as imported the package died at load with
//! `ReferenceError: $Elm$Kernel$Markdown$toHtml is not defined` — the module
//! body assigns it to `Markdown.toHtmlWith` at the top level. alm now vendors
//! the same `marked` build elm/markdown does and drives it through alm's
//! managed-widget vdom node, so the HTML matches elm's byte for byte.
//!
//! The package's Elm source is inlined here rather than read from the real
//! ELM_HOME so the test stands alone (same approach as `bytes_test`).

use alm_compiler::{generate, project};
use std::process::Command;
use std::sync::Mutex;

mod common;

/// ELM_HOME is process-global; serialize the tests that set it.
static ELM_HOME_LOCK: Mutex<()> = Mutex::new(());

/// elm-explorations/markdown 1.0.0's `Markdown.elm`, doc comments trimmed.
const MARKDOWN_ELM: &str = r#"module Markdown exposing
  ( toHtml
  , Options, defaultOptions, toHtmlWith
  )

import Elm.Kernel.Markdown
import Html exposing (Html, Attribute)


toHtml : List (Attribute msg) -> String -> Html msg
toHtml =
  toHtmlWith defaultOptions


type alias Options =
  { githubFlavored : Maybe { tables : Bool, breaks : Bool }
  , defaultHighlighting : Maybe String
  , sanitize : Bool
  , smartypants : Bool
  }


defaultOptions : Options
defaultOptions =
  { githubFlavored = Just { tables = False, breaks = False }
  , defaultHighlighting = Nothing
  , sanitize = True
  , smartypants = False
  }


toHtmlWith : Options -> List (Attribute msg) -> String -> Html msg
toHtmlWith =
  Elm.Kernel.Markdown.toHtml
"#;

/// The document the widget builds its `<div>` from: alm's vdom captures
/// `document` when the bundle loads, and all the widget needs is an element
/// whose `innerHTML` can be written and read back.
const DOC_SHIM: &str = r#"
global.document = {
    createElement: function () {
        return { _html: '', set innerHTML(v) { this._html = v; }, get innerHTML() { return this._html; } };
    }
};
"#;

fn render(markdown_call: &str) -> String {
    let _guard = ELM_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = common::test_dir("alm-markdown", "render");
    let src = dir.join("src");
    let pkg = dir.join("elm-home/0.19.1/packages/elm-explorations/markdown/1.0.0");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"], "elm-version": "0.19.1",
             "dependencies": { "direct": { "elm-explorations/markdown": "1.0.0" }, "indirect": {} },
             "test-dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("elm.json"),
        r#"{ "type": "package", "name": "elm-explorations/markdown", "summary": "m",
             "license": "BSD-3-Clause", "version": "1.0.0", "exposed-modules": ["Markdown"],
             "elm-version": "0.19.0 <= v < 0.20.0",
             "dependencies": { "elm/core": "1.0.0 <= v < 2.0.0", "elm/html": "1.0.0 <= v < 2.0.0" },
             "test-dependencies": {} }"#,
    )
    .unwrap();
    std::fs::write(pkg.join("src/Markdown.elm"), MARKDOWN_ELM).unwrap();
    std::fs::write(
        src.join("Main.elm"),
        format!("module Main exposing (main)\n\nimport Html exposing (Html)\nimport Markdown\n\n\nmain : Html msg\nmain =\n    {markdown_call}\n"),
    )
    .unwrap();

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
            "{DOC_SHIM}\nvar widget = require({:?}).Elm.Main.main;\
             process.stdout.write(widget.render(widget.model).innerHTML);",
            bundle.display()
        ))
        .env_remove("FORCE_COLOR")
        .output()
        .expect("run node");
    assert!(
        out.status.success(),
        "node failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Checked against `elm make` on the same source: elm renders exactly this.
#[test]
fn renders_the_same_html_as_elm() {
    let html = render(
        r##"Markdown.toHtml [] "# Title\n\nSome *em* and `code` and a [link](http://x.y).\n\n- a\n- b\n""##,
    );
    assert_eq!(
        html,
        "<h1 id=\"title\">Title</h1>\n\
         <p>Some <em>em</em> and <code>code</code> and a <a href=\"http://x.y\">link</a>.</p>\n\
         <ul>\n<li>a</li>\n<li>b</li>\n</ul>\n"
    );
}

/// `defaultOptions` sanitizes, so embedded HTML is escaped rather than passed
/// through — the option record has to reach the parser, not just the string.
#[test]
fn default_options_sanitize() {
    let html = render(r#"Markdown.toHtml [] "<script>alert(1)</script>""#);
    assert!(!html.contains("<script>"), "sanitize option was not applied: {html}");
    assert!(html.contains("&lt;script&gt;"), "expected escaped html, got: {html}");
}

/// Turning sanitation off lets raw HTML through, which pins the other
/// direction of the same option plumbing.
#[test]
fn sanitize_can_be_turned_off() {
    let html = render(
        "Markdown.toHtmlWith { defaultOptions | sanitize = False } [] \"<b>hi</b>\"\n\n\ndefaultOptions : Markdown.Options\ndefaultOptions =\n    Markdown.defaultOptions",
    );
    assert!(html.contains("<b>hi</b>"), "raw html should pass through: {html}");
}
