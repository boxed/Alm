//! Test.Html introspection: the `HtmlAsJson` kernel translates alm's virtual
//! dom into the elm/virtual-dom JSON shape that elm-explorations/test's decoder
//! (`Test.Html.Query`/`Selector`) expects. The full Test package can't be
//! pulled into the single-module inline harness, so we exercise the kernel
//! `toJson` translation directly (mirroring `Test.Html.Internal.Inert.toJson`)
//! and assert the exact node/facts shape the decoder reads. End-to-end PASS/FAIL
//! parity against the official compiler is covered by
//! `.registry/tests/diag.py rundis/elm-bootstrap 5.2.0`.

mod common;

/// Compile a module whose `main` is some Html, then in node call the kernel
/// `toJson` on it and print the JSON. The kernel value and the compiled `main`
/// live inside the bundle's IIFE, so we splice a line exposing them onto the
/// `Elm` object just before it is exported.
fn to_json(body: &str) -> String {
    let source = format!("module Test exposing (..)\n\nimport Html\nimport Html.Attributes\nimport Html.Events\nimport Html.Keyed\n\n{}", body);
    let javascript = common::compile_single_no_dce("Test.elm", &source);
    let javascript = javascript.replace(
        "if (typeof module !== 'undefined') { module.exports = Elm; }",
        "Elm._toJson = $Elm$Kernel$HtmlAsJson$toJson; Elm._html = $Test$main;\nif (typeof module !== 'undefined') { module.exports = Elm; }",
    );
    let js_path = common::write_js("html-query", &javascript);
    common::run_node(
        &format!(
            "var Elm = require({:?});\nconsole.log(JSON.stringify(Elm._toJson(Elm._html)));",
            js_path.to_str().unwrap()
        ),
        &javascript,
    )
}

#[test]
fn translates_nodes_facts_and_children() {
    let json = to_json(
        "main =\n    Html.div\n        [ Html.Attributes.class \"container\", Html.Attributes.id \"main\", Html.Attributes.style \"color\" \"red\" ]\n        [ Html.span [ Html.Attributes.class \"label\", Html.Events.onClick () ] [ Html.text \"hi\" ]\n        , Html.node \"custom-el\" [ Html.Attributes.attribute \"data-x\" \"y\" ] []\n        ]",
    );

    // Root element: node tag 1, tag name in `c`, facts in `d`, kids in `e`,
    // descendant count in `b`.
    assert!(json.contains(r#""$":1"#), "no element node: {}", json);
    assert!(json.contains(r#""c":"div""#), "no div tag: {}", json);
    // Classes are applied as the top-level `className` property (matching
    // elm/virtual-dom), which is where the test decoder reads them.
    assert!(json.contains(r#""className":"container""#), "no className: {}", json);
    // Non-class attributes live in the a3 bucket.
    assert!(json.contains(r#""a3":{"#), "no attr bucket: {}", json);
    assert!(json.contains(r#""id":"main""#), "no id attr: {}", json);
    // Style bucket a1.
    assert!(json.contains(r#""a1":{"color":"red"}"#), "no style bucket: {}", json);
    // Nested span with its own className.
    assert!(json.contains(r#""className":"label""#), "no nested className: {}", json);
    // Event bucket a0 keyed by event name.
    assert!(json.contains(r#""a0":{"click""#), "no event bucket: {}", json);
    // Text node: tag 0, text under `a`.
    assert!(json.contains(r#"{"$":0,"a":"hi"}"#), "no text node: {}", json);
    // Custom element tag preserved, arbitrary attribute in a3.
    assert!(json.contains(r#""c":"custom-el""#), "no custom tag: {}", json);
    assert!(json.contains(r#""data-x":"y""#), "no data attr: {}", json);
    // Every node carries an integer descendant count `b`.
    assert!(json.contains(r#""b":"#), "no descendant count: {}", json);
}

#[test]
fn translates_keyed_and_mapped_nodes() {
    let json = to_json(
        "main =\n    Html.map identity <|\n        Html.Keyed.node \"ul\"\n            []\n            [ ( \"k1\", Html.li [ Html.Attributes.class \"item\" ] [ Html.text \"a\" ] )\n            , ( \"k2\", Html.li [] [ Html.text \"b\" ] )\n            ]",
    );

    // Tagger (Html.map): node tag 4, wrapping the child under `k`.
    assert!(json.contains(r#""$":4"#), "no tagger node: {}", json);
    assert!(json.contains(r#""k":"#), "no tagger child field: {}", json);
    // Keyed node: tag 2, children are [key, node] pairs with the node under `b`.
    assert!(json.contains(r#""$":2"#), "no keyed node: {}", json);
    assert!(json.contains(r#""c":"ul""#), "no ul tag: {}", json);
    assert!(json.contains(r#""a":"k1""#), "no key: {}", json);
    assert!(json.contains(r#""className":"item""#), "no keyed child className: {}", json);
    assert!(json.contains(r#"{"$":0,"a":"a"}"#), "no keyed text: {}", json);
}
