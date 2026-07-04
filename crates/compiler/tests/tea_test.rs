//! The Elm Architecture tests: compile Browser.sandbox programs and drive
//! them under node with a minimal DOM shim — clicking buttons, typing into
//! inputs, and asserting on the rendered tree.

mod common;

/// A tiny DOM implementation with just enough surface for the alm virtual
/// DOM: createElement/createTextNode, appendChild/replaceChild/removeChild,
/// get/set/removeAttribute, addEventListener, and style objects.
const DOM_SHIM: &str = r#"
function TextNode(text) {
    this.nodeType = 3;
    this._text = text;
    this.parentNode = null;
}
Object.defineProperty(TextNode.prototype, 'textContent', {
    get: function () { return this._text; },
    set: function (v) { this._text = v; }
});

function Element(tag) {
    this.nodeType = 1;
    this.tagName = tag.toUpperCase();
    this.childNodes = [];
    this.parentNode = null;
    this.style = {};
    this._attributes = {};
    this._listeners = {};
    this.ownerDocument = document;
}
Element.prototype.appendChild = function (child) {
    if (child.parentNode) { child.parentNode.removeChild(child); }
    child.parentNode = this;
    this.childNodes.push(child);
    return child;
};
Element.prototype.removeChild = function (child) {
    var i = this.childNodes.indexOf(child);
    if (i > -1) { this.childNodes.splice(i, 1); child.parentNode = null; }
    return child;
};
Element.prototype.replaceChild = function (newChild, oldChild) {
    var i = this.childNodes.indexOf(oldChild);
    if (i > -1) {
        if (newChild.parentNode) { newChild.parentNode.removeChild(newChild); }
        this.childNodes[i] = newChild;
        newChild.parentNode = this;
        oldChild.parentNode = null;
    }
    return oldChild;
};
Element.prototype.setAttribute = function (k, v) { this._attributes[k] = String(v); };
Element.prototype.getAttribute = function (k) {
    return k in this._attributes ? this._attributes[k] : null;
};
Element.prototype.removeAttribute = function (k) { delete this._attributes[k]; };
Element.prototype.addEventListener = function (name, fn) {
    (this._listeners[name] = this._listeners[name] || []).push(fn);
};
Element.prototype.removeEventListener = function (name, fn) {
    var fns = this._listeners[name] || [];
    var i = fns.indexOf(fn);
    if (i > -1) { fns.splice(i, 1); }
};
Object.defineProperty(Element.prototype, 'textContent', {
    get: function () {
        return this.childNodes.map(function (c) { return c.textContent; }).join('');
    }
});

var document = {
    createElement: function (tag) { return new Element(tag); },
    createTextNode: function (text) { return new TextNode(text); }
};

// Test helpers.
function fire(node, eventName, eventProps) {
    var event = Object.assign({ target: node, preventDefault: function () {} }, eventProps);
    (node._listeners[eventName] || []).slice().forEach(function (fn) { fn(event); });
}
function find(node, predicate) {
    if (node.nodeType === 1) {
        if (predicate(node)) { return node; }
        for (var i = 0; i < node.childNodes.length; i++) {
            var found = find(node.childNodes[i], predicate);
            if (found) { return found; }
        }
    }
    return null;
}
function byTag(root, tag) {
    return find(root, function (n) { return n.tagName === tag.toUpperCase(); });
}
function byText(root, text) {
    return find(root, function (n) { return n.textContent === text; });
}
"#;

/// Compile an Elm program (module `Main`), boot it with the DOM shim, run
/// the given driver script, and return its stdout lines.
fn run_app(elm: &str, driver: &str) -> Vec<String> {
    let javascript = common::compile_single("Main.elm", elm);
    let js_path = common::write_js("tea", &javascript);
    let harness = format!(
        "{shim}\nvar Elm = require({path:?});\nvar host = document.createElement('div');\nvar mount = document.createElement('div');\nhost.appendChild(mount);\nvar app = Elm.Main.main.init({{ node: mount }});\nvar root = host.childNodes[0];\n{driver}\n",
        shim = DOM_SHIM,
        path = js_path.to_str().unwrap(),
        driver = driver
    );
    common::run_node(&harness, &javascript)
        .lines()
        .map(str::to_string)
        .collect()
}

const COUNTER: &str = r#"module Main exposing (main)

import Browser
import Html exposing (Html, button, div, text)
import Html.Events exposing (onClick)


type Msg
    = Increment
    | Decrement


type alias Model =
    Int


update : Msg -> Model -> Model
update msg model =
    case msg of
        Increment ->
            model + 1

        Decrement ->
            model - 1


view : Model -> Html Msg
view model =
    div []
        [ button [ onClick Decrement ] [ text "-" ]
        , div [] [ text (String.fromInt model) ]
        , button [ onClick Increment ] [ text "+" ]
        ]


main : Program () Model Msg
main =
    Browser.sandbox { init = 0, update = update, view = view }
"#;

#[test]
fn counter_renders_and_updates() {
    let output = run_app(
        COUNTER,
        r#"
console.log(root.textContent);
var plus = byText(root, '+');
fire(plus, 'click');
fire(plus, 'click');
fire(plus, 'click');
console.log(root.textContent);
var minus = byText(root, '-');
fire(minus, 'click');
console.log(root.textContent);
"#,
    );
    assert_eq!(output, vec!["-0+", "-3+", "-2+"]);
}

#[test]
fn text_input_flows_through_the_model() {
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, div, input, text)
import Html.Attributes exposing (placeholder, value)
import Html.Events exposing (onInput)


type Msg
    = Change String


update : Msg -> String -> String
update (Change s) _ =
    s


view : String -> Html Msg
view model =
    div []
        [ input [ placeholder "type here", value model, onInput Change ] []
        , div [] [ text (String.reverse model) ]
        ]


main : Program () String Msg
main =
    Browser.sandbox { init = "", update = update, view = view }
"#;
    let output = run_app(
        app,
        r#"
var box = byTag(root, 'input');
console.log(box.getAttribute('placeholder'));
box.value = 'hello';
fire(box, 'input');
console.log(root.textContent);
console.log(byTag(root, 'input').value);
"#,
    );
    assert_eq!(output, vec!["type here", "olleh", "hello"]);
}

#[test]
fn conditional_rendering_adds_and_removes_nodes() {
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, button, div, li, text, ul)
import Html.Events exposing (onClick)


type Msg
    = Add


update : Msg -> List String -> List String
update Add items =
    "item" :: items


view : List String -> Html Msg
view items =
    div []
        [ button [ onClick Add ] [ text "add" ]
        , if List.isEmpty items then
            text "empty"

          else
            ul [] (List.map (\item -> li [] [ text item ]) items)
        ]


main : Program () (List String) Msg
main =
    Browser.sandbox { init = [], update = update, view = view }
"#;
    let output = run_app(
        app,
        r#"
console.log(root.textContent);
fire(byText(root, 'add'), 'click');
fire(byText(root, 'add'), 'click');
console.log(byTag(root, 'ul').childNodes.length);
console.log(root.textContent);
"#,
    );
    assert_eq!(output, vec!["addempty", "2", "additemitem"]);
}

#[test]
fn html_map_wraps_child_messages() {
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, button, div, text)
import Html.Events exposing (onClick)


type ChildMsg
    = Poke


type Msg
    = FromChild ChildMsg


childView : Html ChildMsg
childView =
    button [ onClick Poke ] [ text "poke" ]


update : Msg -> Int -> Int
update (FromChild Poke) n =
    n + 1


view : Int -> Html Msg
view n =
    div []
        [ Html.map FromChild childView
        , text (String.fromInt n)
        ]


main : Program () Int Msg
main =
    Browser.sandbox { init = 0, update = update, view = view }
"#;
    let output = run_app(
        app,
        r#"
fire(byText(root, 'poke'), 'click');
fire(byText(root, 'poke'), 'click');
console.log(root.textContent);
"#,
    );
    assert_eq!(output, vec!["poke2"]);
}

#[test]
fn browser_element_runs_with_flags() {
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, div, text)


type alias Model =
    Int


init : () -> ( Model, Cmd msg )
init _ =
    ( 42, Cmd.none )


update : msg -> Model -> ( Model, Cmd msg )
update _ model =
    ( model, Cmd.none )


view : Model -> Html msg
view model =
    div [] [ text (String.fromInt model) ]


main : Program () Model msg
main =
    Browser.element
        { init = init
        , update = update
        , subscriptions = \_ -> Sub.none
        , view = view
        }
"#;
    let output = run_app(app, "console.log(root.textContent);");
    assert_eq!(output, vec!["42"]);
}

#[test]
fn attributes_and_styles_render() {
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, div, text)
import Html.Attributes exposing (class, id, style)


view : () -> Html msg
view _ =
    div [ class "wrapper", id "app", style "color" "red" ] [ text "styled" ]


main : Program () () msg
main =
    Browser.sandbox { init = (), update = \_ m -> m, view = view }
"#;
    let output = run_app(
        app,
        r#"
console.log(root.getAttribute('class'));
console.log(root.getAttribute('id'));
console.log(root.style.color);
"#,
    );
    assert_eq!(output, vec!["wrapper", "app", "red"]);
}

#[test]
fn vdom_patch_paths() {
    // Lazy nodes: unchanged args skip re-render; changed args re-render.
    // Tag changes force node replacement. Html.map survives patching.
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, button, div, em, span, strong, text)
import Html.Events exposing (onClick)
import Html.Lazy


type Msg
    = Bump
    | Wrapped InnerMsg


type InnerMsg
    = Inner


badge : Int -> Html msg
badge n =
    span [] [ text ("b" ++ String.fromInt n) ]


view : { count : Int, inner : Int } -> Html Msg
view model =
    div []
        [ button [ onClick Bump ] [ text "go" ]
        , Html.Lazy.lazy badge (model.count // 2)
        , if modBy 2 model.count == 0 then
            strong [] [ text "even" ]

          else
            em [] [ text "odd" ]
        , Html.map Wrapped (button [ onClick Inner ] [ text ("inner" ++ String.fromInt model.inner) ])
        ]


update : Msg -> { count : Int, inner : Int } -> { count : Int, inner : Int }
update msg model =
    case msg of
        Bump ->
            { model | count = model.count + 1 }

        Wrapped Inner ->
            { model | inner = model.inner + 1 }


main : Program () { count : Int, inner : Int } Msg
main =
    Browser.sandbox { init = { count = 0, inner = 0 }, update = update, view = view }
"#;
    let output = run_app(
        app,
        r#"
console.log(root.textContent);
fire(byText(root, 'go'), 'click');           // count 1: lazy arg still 0, strong -> em
console.log(root.textContent);
fire(byText(root, 'go'), 'click');           // count 2: lazy arg 1, em -> strong
console.log(root.textContent);
var inner = byText(root, 'inner0');
fire(inner, 'click');
console.log(root.textContent);
"#,
    );
    assert_eq!(
        output,
        vec!["gob0eveninner0", "gob0oddinner0", "gob1eveninner0", "gob1eveninner1"]
    );
}

#[test]
fn events_without_prevent_default_support() {
    // Handlers must only call preventDefault when the event offers it and
    // the options ask for it.
    let output = run_app(
        COUNTER,
        r#"
var plus = byText(root, '+');
// A bare event object: no preventDefault, no stopPropagation.
(plus._listeners['click'] || []).forEach(function (fn) { fn({ target: plus }); });
console.log(root.textContent);
"#,
    );
    assert_eq!(output, vec!["-1+"]);
}

#[test]
fn browser_document_runs_under_the_shim() {
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (button, div, text)
import Html.Events exposing (onClick)


type Msg
    = Bump


view : Int -> Browser.Document Msg
view n =
    { title = "count:" ++ String.fromInt n
    , body =
        [ div [] [ text ("n=" ++ String.fromInt n) ]
        , button [ onClick Bump ] [ text "up" ]
        ]
    }


main : Program () Int Msg
main =
    Browser.document
        { init = \_ -> ( 0, Cmd.none )
        , update = \Bump n -> ( n + 1, Cmd.none )
        , subscriptions = \_ -> Sub.none
        , view = view
        }
"#;
    let output = run_app_document(
        app,
        r#"
console.log(document.title);
console.log(document.body.textContent);
fire(byText(document.body, 'up'), 'click');
console.log(document.title);
console.log(document.body.textContent);
"#,
    );
    assert_eq!(
        output,
        vec!["count:0", "n=0up", "count:1", "n=1up"]
    );
}

/// Like run_app but for Browser.document programs: the shim gains a body
/// and a title, and the app mounts itself.
fn run_app_document(elm: &str, driver: &str) -> Vec<String> {
    let javascript = common::compile_single("Main.elm", elm);
    let js_path = common::write_js("tea-doc", &javascript);
    let harness = format!(
        "{shim}\ndocument.body = document.createElement('body');\ndocument.title = '';\nvar Elm = require({path:?});\nvar app = Elm.Main.main.init({{}});\n{driver}\n",
        shim = DOM_SHIM,
        path = js_path.to_str().unwrap(),
        driver = driver
    );
    common::run_node(&harness, &javascript)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn attribute_and_listener_dedup_keys() {
    // Multiple attributes and multiple listeners on one node: removing one
    // must not disturb the others across a patch.
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, button, div, input, text)
import Html.Attributes exposing (class, id, style)
import Html.Events exposing (onClick, onInput)


type Msg
    = Toggle
    | Typed String


type alias Model =
    { fancy : Bool, typed : String }


view : Model -> Html Msg
view model =
    div []
        [ button
            (if model.fancy then
                [ class "fancy", id "b", style "color" "red", onClick Toggle ]

             else
                [ id "b", onClick Toggle ]
            )
            [ text "toggle" ]
        , input [ id "i", onInput Typed, onClick Toggle ] []
        , div [ id "out" ] [ text (model.typed ++ "/" ++ Debug.toString model.fancy) ]
        ]


main : Program () Model Msg
main =
    Browser.sandbox
        { init = { fancy = True, typed = "" }
        , update =
            \msg m ->
                case msg of
                    Toggle ->
                        { m | fancy = not m.fancy }

                    Typed s ->
                        { m | typed = s }
        , view = view
        }
"#;
    let output = run_app(
        app,
        r#"
var b = find(root, function (n) { return n._attributes && n._attributes.id === 'b'; });
console.log(b.getAttribute('class') + '/' + b.style.color);
fire(b, 'click');   // fancy -> false: class and style must both go away
console.log(b.getAttribute('class') + '/' + (b.style.color || 'none'));
var i = find(root, function (n) { return n._attributes && n._attributes.id === 'i'; });
i.value = 'hey';
fire(i, 'input');   // typed model updates; the click listener must still work
console.log(byText(root, 'hey/False') !== null);
fire(i, 'click');
console.log(byText(root, 'hey/True') !== null);
"#,
    );
    assert_eq!(output, vec!["fancy/red", "null/none", "true", "true"]);
}

#[test]
fn lazy_and_map_toggle_with_tag_assertions() {
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, button, div, em, span, strong, text)
import Html.Events exposing (onClick)
import Html.Lazy


type Msg
    = Flip
    | Sub SubMsg


type SubMsg
    = Poke


part : Bool -> Html Msg
part flag =
    if flag then
        Html.Lazy.lazy (\_ -> strong [] [ text "L" ]) ()

    else
        em [] [ text "P" ]


mapped : Bool -> Html Msg
mapped flag =
    if flag then
        Html.map Sub (span [ onClick Poke ] [ text "M" ])

    else
        span [] [ text "M" ]


view : { flag : Bool, pokes : Int } -> Html Msg
view model =
    div []
        [ button [ onClick Flip ] [ text "flip" ]
        , part model.flag
        , mapped model.flag
        , text (String.fromInt model.pokes)
        ]


main : Program () { flag : Bool, pokes : Int } Msg
main =
    Browser.sandbox
        { init = { flag = True, pokes = 0 }
        , update =
            \msg m ->
                case msg of
                    Flip ->
                        { m | flag = not m.flag }

                    Sub Poke ->
                        { m | pokes = m.pokes + 1 }
        , view = view
        }
"#;
    let output = run_app(
        app,
        r#"
console.log((byTag(root, 'strong') !== null) + '/' + (byTag(root, 'em') !== null));
fire(byText(root, 'flip'), 'click');   // lazy -> plain, mapped -> unmapped
console.log((byTag(root, 'strong') !== null) + '/' + (byTag(root, 'em') !== null));
fire(byText(root, 'flip'), 'click');   // back again
console.log((byTag(root, 'strong') !== null) + '/' + (byTag(root, 'em') !== null));
fire(byText(root, 'M'), 'click');      // mapped click routes through Sub
console.log(root.textContent.slice(-1));
"#,
    );
    assert_eq!(output, vec!["true/false", "false/true", "true/false", "1"]);
}

#[test]
fn svg_renders_under_shim_without_namespace_support() {
    // The shim has no createElementNS; the renderer must fall back.
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, div)
import Svg
import Svg.Attributes as A


view : () -> Html msg
view _ =
    div [] [ Svg.svg [ A.viewBox "0 0 1 1" ] [ Svg.circle [ A.r "1" ] [] ] ]


main : Program () () msg
main =
    Browser.sandbox { init = (), update = \_ m -> m, view = view }
"#;
    let output = run_app(app, "console.log(byTag(root, 'svg') !== null && byTag(root, 'circle') !== null);");
    assert_eq!(output, vec!["true"]);
}

#[test]
fn plain_clicks_do_not_prevent_default() {
    let output = run_app(
        COUNTER,
        r#"
var plus = byText(root, '+');
var prevented = false;
(plus._listeners['click'] || []).forEach(function (fn) {
    fn({ target: plus, preventDefault: function () { prevented = true; } });
});
console.log(root.textContent + '/' + prevented);
"#,
    );
    assert_eq!(output, vec!["-1+/false"]);
}

#[test]
fn style_and_attribute_with_same_key_are_distinct() {
    let app = r#"module Main exposing (main)

import Browser
import Html exposing (Html, button, div, text)
import Html.Attributes exposing (attribute, style)
import Html.Events exposing (onClick)


type Msg
    = Flip


view : Bool -> Html Msg
view keepBoth =
    div []
        [ button [ onClick Flip ] [ text "flip" ]
        , if keepBoth then
            div [ attribute "width" "20", style "width" "10px" ] [ text "x" ]

          else
            div [ style "width" "10px" ] [ text "x" ]
        ]


main : Program () Bool Msg
main =
    Browser.sandbox { init = True, update = \_ m -> not m, view = view }
"#;
    let output = run_app(
        app,
        r#"
var x = byText(root, 'x');
console.log(x.getAttribute('width') + '/' + x.style.width);
fire(byText(root, 'flip'), 'click');   // attribute goes away, style stays
console.log(x.getAttribute('width') + '/' + x.style.width);
"#,
    );
    assert_eq!(output, vec!["20/10px", "null/10px"]);
}
