//! The Elm Architecture tests: compile Browser.sandbox programs and drive
//! them under node with a minimal DOM shim — clicking buttons, typing into
//! inputs, and asserting on the rendered tree.

use std::process::Command;

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
    let javascript = match alm_compiler::compile(elm) {
        Ok(js) => js,
        Err(reports) => panic!(
            "compilation failed:\n{}",
            reports
                .iter()
                .map(|r| r.render("Main.elm", elm))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };

    let dir = std::env::temp_dir().join(format!(
        "alm-tea-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let js_path = dir.join("app.js");
    std::fs::write(&js_path, &javascript).unwrap();

    let harness = format!(
        "{shim}\nvar Elm = require({path:?});\nvar host = document.createElement('div');\nvar mount = document.createElement('div');\nhost.appendChild(mount);\nvar app = Elm.Main.main.init({{ node: mount }});\nvar root = host.childNodes[0];\n{driver}\n",
        shim = DOM_SHIM,
        path = js_path.to_str().unwrap(),
        driver = driver
    );

    let output = Command::new("node")
        .arg("-e")
        .arg(&harness)
        .output()
        .expect("failed to run node");
    if !output.status.success() {
        panic!(
            "node failed:\n{}\n\ngenerated JS:\n{}",
            String::from_utf8_lossy(&output.stderr),
            javascript
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
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
