//! Performance-lint tests: a keyed list without `Html.Lazy` should draw a hint;
//! a lazy one, or a non-keyed list, should not.

mod common;

use alm_compiler::{lint, project};

fn hints(name: &str, source: &str) -> Vec<String> {
    let dir = common::test_dir("alm-lint", name);
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");
    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!("check failed:\n{}", errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n"))
    });
    lint::lint(&checked.modules, &checked.sources)
        .iter()
        .map(|w| w.render())
        .collect()
}

const HEAD: &str = "module Test exposing (main)\n\n\
     import Browser\n\
     import Html exposing (..)\n\
     import Html.Attributes exposing (class)\n\
     import Html.Events exposing (onClick)\n\
     import Html.Keyed as Keyed\n\
     import Html.Lazy exposing (lazy2)\n\n\
     type alias Row = { id : Int, label : String }\n\
     type Msg = Sel Int\n\
     row : Int -> Row -> ( String, Html Msg )\n\
     row sel r = ( String.fromInt r.id, tr [ class (if r.id == sel then \"x\" else \"\") ] [ text r.label ] )\n\
     rowLazy : Int -> Row -> ( String, Html Msg )\n\
     rowLazy sel r = ( String.fromInt r.id, lazy2 rowInner (r.id == sel) r )\n\
     rowInner : Bool -> Row -> Html Msg\n\
     rowInner s r = tr [ class (if s then \"x\" else \"\") ] [ text r.label ]\n\
     up : Msg -> List Row -> List Row\n\
     up _ rs = rs\n\n";

fn prog(view_body: &str) -> String {
    format!(
        "{HEAD}view : List Row -> Html Msg\nview rows =\n    {view_body}\n\nmain : Program () (List Row) Msg\nmain = Browser.sandbox {{ init = [], update = up, view = view }}\n"
    )
}

#[test]
fn keyed_without_lazy_warns() {
    let h = hints("keyed_nolazy", &prog("Keyed.node \"table\" [] (List.map (row 0) rows)"));
    assert_eq!(h.len(), 1, "expected one hint, got: {h:?}");
    assert!(h[0].contains("PERFORMANCE HINT"), "{}", h[0]);
    assert!(h[0].contains("Html.Lazy"), "{}", h[0]);
}

#[test]
fn keyed_with_lazy_is_clean() {
    let h = hints("keyed_lazy", &prog("Keyed.node \"table\" [] (List.map (rowLazy 0) rows)"));
    assert!(h.is_empty(), "expected no hint, got: {h:?}");
}

#[test]
fn keyed_with_inline_lazy_is_clean() {
    let h = hints(
        "keyed_lazy_inline",
        &prog("Keyed.node \"table\" [] (List.map (\\r -> ( String.fromInt r.id, lazy2 rowInner False r )) rows)"),
    );
    assert!(h.is_empty(), "expected no hint, got: {h:?}");
}

#[test]
fn non_keyed_list_is_clean() {
    // A plain (non-keyed) list draws no hint — the lint targets keyed lists,
    // the canonical large-dynamic-list case.
    let h = hints("nonkeyed", &prog("div [] (List.map (\\r -> tr [] [ text r.label ]) rows)"));
    assert!(h.is_empty(), "expected no hint, got: {h:?}");
}
