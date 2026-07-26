//! Pins `Html.Attributes` to elm/html 1.0.0's published surface.
//!
//! alm models Html.Attributes with tables rather than compiling elm/html's
//! source, so the tables can silently drift from the package: at one point
//! seven helpers were missing (`contextmenu`, `dropzone`, `form`, `itemprop`,
//! `preload`, `pubdate`, `reversed`), three that elm 0.19 removed were still
//! accepted (`charset`, `content`, `httpEquiv`), and `accesskey` took a String
//! where elm takes a Char. This test compares the whole module against the
//! signatures in elm/html 1.0.0's docs.json, so a name or a type can only
//! change deliberately.

use alm_compiler::builtins;

mod common;

/// Every value elm/html 1.0.0 exposes from Html.Attributes, with the type it
/// is published under (transcribed from that release's docs.json).
const ELM_HTML_ATTRIBUTES: &[(&str, &str)] = &[
    ("accept", "String -> Attribute msg"),
    ("acceptCharset", "String -> Attribute msg"),
    ("accesskey", "Char -> Attribute msg"),
    ("action", "String -> Attribute msg"),
    ("align", "String -> Attribute msg"),
    ("alt", "String -> Attribute msg"),
    ("attribute", "String -> String -> Attribute msg"),
    ("autocomplete", "Bool -> Attribute msg"),
    ("autofocus", "Bool -> Attribute msg"),
    ("autoplay", "Bool -> Attribute msg"),
    ("checked", "Bool -> Attribute msg"),
    ("cite", "String -> Attribute msg"),
    ("class", "String -> Attribute msg"),
    ("classList", "List ( String, Bool ) -> Attribute msg"),
    ("cols", "Int -> Attribute msg"),
    ("colspan", "Int -> Attribute msg"),
    ("contenteditable", "Bool -> Attribute msg"),
    ("contextmenu", "String -> Attribute msg"),
    ("controls", "Bool -> Attribute msg"),
    ("coords", "String -> Attribute msg"),
    ("datetime", "String -> Attribute msg"),
    ("default", "Bool -> Attribute msg"),
    ("dir", "String -> Attribute msg"),
    ("disabled", "Bool -> Attribute msg"),
    ("download", "String -> Attribute msg"),
    ("draggable", "String -> Attribute msg"),
    ("dropzone", "String -> Attribute msg"),
    ("enctype", "String -> Attribute msg"),
    ("for", "String -> Attribute msg"),
    ("form", "String -> Attribute msg"),
    ("headers", "String -> Attribute msg"),
    ("height", "Int -> Attribute msg"),
    ("hidden", "Bool -> Attribute msg"),
    ("href", "String -> Attribute msg"),
    ("hreflang", "String -> Attribute msg"),
    ("id", "String -> Attribute msg"),
    ("ismap", "Bool -> Attribute msg"),
    ("itemprop", "String -> Attribute msg"),
    ("kind", "String -> Attribute msg"),
    ("lang", "String -> Attribute msg"),
    ("list", "String -> Attribute msg"),
    ("loop", "Bool -> Attribute msg"),
    ("manifest", "String -> Attribute msg"),
    ("map", "(a -> msg) -> Attribute a -> Attribute msg"),
    ("max", "String -> Attribute msg"),
    ("maxlength", "Int -> Attribute msg"),
    ("media", "String -> Attribute msg"),
    ("method", "String -> Attribute msg"),
    ("min", "String -> Attribute msg"),
    ("minlength", "Int -> Attribute msg"),
    ("multiple", "Bool -> Attribute msg"),
    ("name", "String -> Attribute msg"),
    ("novalidate", "Bool -> Attribute msg"),
    ("pattern", "String -> Attribute msg"),
    ("ping", "String -> Attribute msg"),
    ("placeholder", "String -> Attribute msg"),
    ("poster", "String -> Attribute msg"),
    ("preload", "String -> Attribute msg"),
    ("property", "String -> Value -> Attribute msg"),
    ("pubdate", "String -> Attribute msg"),
    ("readonly", "Bool -> Attribute msg"),
    ("rel", "String -> Attribute msg"),
    ("required", "Bool -> Attribute msg"),
    ("reversed", "Bool -> Attribute msg"),
    ("rows", "Int -> Attribute msg"),
    ("rowspan", "Int -> Attribute msg"),
    ("sandbox", "String -> Attribute msg"),
    ("scope", "String -> Attribute msg"),
    ("selected", "Bool -> Attribute msg"),
    ("shape", "String -> Attribute msg"),
    ("size", "Int -> Attribute msg"),
    ("spellcheck", "Bool -> Attribute msg"),
    ("src", "String -> Attribute msg"),
    ("srcdoc", "String -> Attribute msg"),
    ("srclang", "String -> Attribute msg"),
    ("start", "Int -> Attribute msg"),
    ("step", "String -> Attribute msg"),
    ("style", "String -> String -> Attribute msg"),
    ("tabindex", "Int -> Attribute msg"),
    ("target", "String -> Attribute msg"),
    ("title", "String -> Attribute msg"),
    ("type_", "String -> Attribute msg"),
    ("usemap", "String -> Attribute msg"),
    ("value", "String -> Attribute msg"),
    ("width", "Int -> Attribute msg"),
    ("wrap", "String -> Attribute msg"),
];

#[test]
fn html_attributes_matches_elm_html() {
    let mut missing = Vec::new();
    let mut wrong_type = Vec::new();
    for (name, signature) in ELM_HTML_ATTRIBUTES {
        match builtins::lookup_value("Html.Attributes", name) {
            None => missing.push(*name),
            Some(v) if v.signature != *signature => {
                wrong_type.push(format!("{name}: alm has `{}`, elm has `{signature}`", v.signature))
            }
            Some(_) => {}
        }
    }
    let known: std::collections::HashSet<&str> =
        ELM_HTML_ATTRIBUTES.iter().map(|(n, _)| *n).collect();
    let extra: Vec<&str> = builtins::values()
        .iter()
        .filter(|v| v.module == "Html.Attributes" && !known.contains(v.name))
        .map(|v| v.name)
        .collect();

    assert!(missing.is_empty(), "Html.Attributes values elm has and alm lacks: {missing:?}");
    assert!(wrong_type.is_empty(), "Html.Attributes signature mismatches:\n{}", wrong_type.join("\n"));
    assert!(extra.is_empty(), "Html.Attributes values alm invents (elm rejects these): {extra:?}");
}

/// Every helper must also be emittable — the JS backend generates one `var`
/// per table entry, and the wasm-gc backend looks each name up in the same
/// tables. A name present in the signature table but absent from the emission
/// tables would compile and then blow up as an undefined reference.
#[test]
fn every_attribute_helper_is_emitted() {
    // The helpers the backends write out by hand rather than from a table.
    const HANDWRITTEN: &[&str] =
        &["attribute", "classList", "map", "property", "style", "autocomplete", "start"];
    let tabled: std::collections::HashSet<&str> = builtins::HTML_STRING_PROPS
        .iter()
        .chain(builtins::HTML_STRING_ATTRS)
        .chain(builtins::HTML_BOOL_ATTRS)
        .chain(builtins::HTML_INT_ATTRS)
        .chain(builtins::HTML_CHAR_ATTRS)
        .map(|(n, _)| *n)
        .collect();
    let unemitted: Vec<&str> = ELM_HTML_ATTRIBUTES
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !tabled.contains(n) && !HANDWRITTEN.contains(n))
        .collect();
    assert!(unemitted.is_empty(), "declared but never emitted: {unemitted:?}");
}

/// The helpers whose emission is off-pattern, checked end to end against the
/// shapes elm/html produces: `accesskey` takes a Char but sets a one-character
/// string property, `start` an Int rendered as a *string* property,
/// `autocomplete` a Bool rendered "on"/"off", `reversed` a plain bool
/// property, and `form` a raw attribute rather than a property.
#[test]
fn off_pattern_attributes_render_like_elm() {
    let javascript = common::compile_single(
        "Attrs.elm",
        r#"module Attrs exposing (view)

import Html exposing (Html, div)
import Html.Attributes as At


view : Html msg
view =
    div [ At.accesskey 'k', At.start 3, At.autocomplete True, At.reversed True, At.form "f1", At.class "c" ] []
"#,
    );
    let path = common::write_js("attrs-offpattern", &javascript);
    let out = common::run_node(
        &format!(
            "var m = require({:?});\
             console.log(m.Attrs.view.attrs.map(function (a) {{ return a.$ + ' ' + a.key + '=' + JSON.stringify(a.val); }}).join('|'));",
            path.display()
        ),
        &javascript,
    );
    assert_eq!(
        out,
        "AProp accessKey=\"k\"|AProp start=\"3\"|AProp autocomplete=\"on\"|AProp reversed=true|AAttr form=\"f1\"|AProp className=\"c\""
    );
}

/// elm/virtual-dom screens the tag names, attribute keys and URIs a program
/// can build at runtime; alm shipped none of that. A `javascript:` URI becomes
/// elm's placeholder alert, `<script>` becomes `<p>`, and an `on*` attribute
/// key is prefixed with `data-`.
#[test]
fn xss_vectors_are_screened_like_elm() {
    let javascript = common::compile_single(
        "Xss.elm",
        r#"module Xss exposing (view)

import Html exposing (Html, a, div, node, text)
import Html.Attributes as At


view : Html msg
view =
    div []
        [ a [ At.href "  jaVa\tscript:alert(1)" ] [ text "x" ]
        , a [ At.href "https://ok" ] [ text "y" ]
        , node "script" [] []
        , div [ At.attribute "onclick" "boom()" ] []
        , div [ At.attribute "formAction" "boom" ] []
        ]
"#,
    );
    let path = common::write_js("attrs-xss", &javascript);
    let out = common::run_node(
        &format!(
            "var m = require({:?});\
             console.log(m.Xss.view.kids.map(function (k) {{ return k.tag + '[' + k.attrs.map(function (a) {{ return a.key + '=' + a.val; }}).join(',') + ']'; }}).join(' '));",
            path.display()
        ),
        &javascript,
    );
    assert_eq!(
        out,
        "a[href=javascript:alert(\"This is an XSS vector. Please use ports or web components instead.\")] \
         a[href=https://ok] p[] div[data-onclick=boom()] div[data-formAction=boom]"
    );
}
