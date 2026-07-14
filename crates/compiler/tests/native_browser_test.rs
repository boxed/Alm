//! Browser programs on the wasm backend, driven by the thin JS shim against a
//! DOM stub, checked against the JS backend (runtime.js) rendering the same
//! program into the same stub. Asserts the two backends produce identical DOM
//! after the initial render and after each scripted event. Requires the
//! wasm32-wasi target and node.

mod common;

use std::process::Command;

use alm_compiler::{generate, project};

/// One scripted interaction, applied identically to both backends.
enum Step {
    /// Fire `event` on the `nth` element with tag `tag`, passing `payload`
    /// (a raw JSON object) as the event value.
    Event { tag: &'static str, nth: usize, event: &'static str, payload: &'static str },
    /// Advance the virtual clock by `ms` (fires due timers / intervals).
    Advance(u64),
    /// Deliver one animation frame.
    Frame,
    /// Send `value` (raw JSON) to the incoming port `name`.
    Port { name: &'static str, value: &'static str },
    /// Fire a document-level event (for `Browser.Events` subscriptions).
    DocEvent { event: &'static str, payload: &'static str },
    /// Resolve the oldest pending HTTP request (status 0 == network error).
    Http { status: u32, body: &'static str },
}

impl Step {
    fn to_json(&self) -> String {
        match self {
            Step::Event { tag, nth, event, payload } => format!(
                r#"{{"kind":"event","tag":"{tag}","nth":{nth},"event":"{event}","payload":{payload}}}"#
            ),
            Step::Advance(ms) => format!(r#"{{"kind":"advance","ms":{ms}}}"#),
            Step::Frame => r#"{"kind":"frame"}"#.to_string(),
            Step::Port { name, value } => format!(r#"{{"kind":"port","name":"{name}","value":{value}}}"#),
            Step::DocEvent { event, payload } => {
                format!(r#"{{"kind":"docevent","event":"{event}","payload":{payload}}}"#)
            }
            Step::Http { status, body } => {
                let escaped = body.replace('\\', "\\\\").replace('"', "\\\"");
                format!(r#"{{"kind":"http","status":{status},"body":"{escaped}"}}"#)
            }
        }
    }
}

fn step(tag: &'static str, nth: usize, event: &'static str, payload: &'static str) -> Step {
    Step::Event { tag, nth, event, payload }
}
fn advance(ms: u64) -> Step {
    Step::Advance(ms)
}
fn frame() -> Step {
    Step::Frame
}
fn port(name: &'static str, value: &'static str) -> Step {
    Step::Port { name, value }
}
fn doc_event(event: &'static str, payload: &'static str) -> Step {
    Step::DocEvent { event, payload }
}
fn http(status: u32, body: &'static str) -> Step {
    Step::Http { status, body }
}

/// Compile `source` with both backends, render each into its own DOM stub, run
/// the `steps`, and return per-backend snapshot sequences (initial render, then
/// one snapshot after each step).
fn run_steps(
    test_name: &str,
    source: &str,
    steps: &[Step],
    flags: &str,
) -> (Vec<String>, Vec<String>) {
    let dir = common::test_dir("alm-browser", test_name);
    let entry = dir.join("Test.elm");
    std::fs::write(&entry, source).expect("write fixture");

    let checked = project::check_project(&entry).unwrap_or_else(|errors| {
        panic!(
            "check failed:\n{}",
            errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
        )
    });

    let bundle = dir.join("bundle.js");
    std::fs::write(&bundle, generate::generate_project(&checked.modules)).expect("write bundle");

    let wasm = dir.join("app.wasm");
    project::compile_project_typed(&entry, &wasm, generate::native::Target::Wasm)
        .unwrap_or_else(|e| {
            panic!(
                "wasm build failed:\n{}",
                e.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
            )
        });

    // Steps as a JSON array (built by hand — no serde dep).
    let steps_json = format!(
        "[{}]",
        steps.iter().map(Step::to_json).collect::<Vec<_>>().join(",")
    );
    let steps_file = dir.join("steps.json");
    std::fs::write(&steps_file, &steps_json).expect("write steps");

    let support = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/browser_support");
    let runner = dir.join("run.cjs");
    std::fs::write(&runner, RUNNER).expect("write runner");

    let output = Command::new("node")
        .arg("--no-warnings")
        .arg(&runner)
        .arg(support)
        .arg(&bundle)
        .arg(&wasm)
        .arg(&steps_file)
        .arg(flags)
        .output()
        .expect("spawn node");
    assert!(
        output.status.success(),
        "runner failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8_lossy(&output.stdout);
    // Snapshots are joined by \x01 (never present in HTML).
    let field = |prefix: &str| -> Vec<String> {
        out.lines()
            .find_map(|l| l.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("runner output missing {prefix:?}:\n{out}"))
            .split('\u{1}')
            .map(str::to_string)
            .collect()
    };
    (field("JS:"), field("WASM:"))
}

/// Assert the two backends agree at every snapshot; optionally check the final
/// snapshot against `expected_final`.
fn assert_interaction(
    test_name: &str,
    source: &str,
    steps: &[Step],
    expected_final: Option<&str>,
) {
    assert_interaction_flags(test_name, source, steps, expected_final, "null");
}

/// Like [`assert_interaction`] but with program flags (a JSON string).
fn assert_interaction_flags(
    test_name: &str,
    source: &str,
    steps: &[Step],
    expected_final: Option<&str>,
    flags: &str,
) {
    let (js, wasm) = run_steps(test_name, source, steps, flags);
    assert_eq!(js.len(), wasm.len(), "snapshot count differs");
    for (i, (j, w)) in js.iter().zip(wasm.iter()).enumerate() {
        assert_eq!(j, w, "backends disagree at snapshot {i}");
    }
    if let Some(exp) = expected_final {
        // The last entry is the outgoing-port pseudo-snapshot ("OUT:..."); the
        // final DOM render is the one before it.
        let final_dom = &js[js.len() - 2];
        assert_eq!(final_dom, exp, "unexpected final render");
    }
}

fn assert_static(test_name: &str, source: &str, expected: Option<&str>) {
    assert_interaction(test_name, source, &[], expected);
}

const RUNNER: &str = r#"
const path = require('path');
const fs = require('fs');
const support = process.argv[2];
const bundle = process.argv[3];
const wasmPath = process.argv[4];
const steps = JSON.parse(fs.readFileSync(process.argv[5], 'utf8'));
const flags = process.argv[6];
const { Document, serializeBody, dispatchEvent, dispatchDocEvent } = require(path.join(support, 'dom_stub.cjs'));
const jsDrv = require(path.join(support, 'js_driver.cjs'));
const wasmDrv = require(path.join(support, 'wasm_driver.cjs'));
const { makeClock } = require(path.join(support, 'clock.cjs'));

// Run pending microtasks (the JS runtime defers its initial Cmd via one).
const flush = () => new Promise((r) => setImmediate(r));

// Collect all elements with the given tag, in document order.
function queryAll(node, tag, acc) {
  for (const c of node.childNodes) {
    if (c.nodeType === 1) {
      if (c.tagName.toLowerCase() === tag.toLowerCase()) acc.push(c);
      queryAll(c, tag, acc);
    }
  }
  return acc;
}

async function applyStep(doc, ctx, s) {
  if (s.kind === 'event') {
    const el = queryAll(doc.body, s.tag, [])[s.nth];
    if (!el) throw new Error('no ' + s.tag + '[' + s.nth + ']');
    dispatchEvent(el, s.event, JSON.parse(JSON.stringify(s.payload)));
  } else if (s.kind === 'advance') {
    ctx.clock.advance(s.ms);
  } else if (s.kind === 'frame') {
    ctx.clock.flushFrame();
  } else if (s.kind === 'port') {
    ctx.sendPort(s.name, s.value);
  } else if (s.kind === 'docevent') {
    dispatchDocEvent(doc, s.event, JSON.parse(JSON.stringify(s.payload)));
  } else if (s.kind === 'http') {
    ctx.resolveHttp(s.status, s.body);
  }
  await flush();
}

async function snapshots(doc, ctx) {
  await flush(); // let the initial Cmd (deferred by the JS runtime) run
  const out = [serializeBody(doc)];
  for (const s of steps) {
    await applyStep(doc, ctx, s);
    out.push(serializeBody(doc));
  }
  return out;
}

(async () => {
  const jdoc = new Document();
  const jctx = jsDrv.start(bundle, jdoc, makeClock(), flags);
  const js = await snapshots(jdoc, jctx);
  if (jctx.restore) jctx.restore();

  const wdoc = new Document();
  const wctx = await wasmDrv.start(wasmPath, wdoc, makeClock(), flags);
  const wasm = await snapshots(wdoc, wctx);

  // Append outgoing-port output as a final pseudo-snapshot so it is diffed too.
  js.push('OUT:' + JSON.stringify(jctx.outgoing || {}));
  wasm.push('OUT:' + JSON.stringify(wctx.outgoing || {}));

  console.log('JS:' + js.join(''));
  console.log('WASM:' + wasm.join(''));
})().catch((e) => { console.error(e && e.stack || e); process.exit(1); });
"#;

// -- static-render tests -----------------------------------------------------

const SANDBOX_STATIC: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, text)
import Html.Attributes exposing (class, id)

type Msg = NoOp

main : Program () Int Msg
main =
    Browser.sandbox { init = 0, update = update, view = view }

update : Msg -> Int -> Int
update _ m = m

view : Int -> Html Msg
view _ =
    div [ class "greeting", id "root" ] [ text "hello world" ]
"#;

#[test]
fn sandbox_static_view_matches_js() {
    assert_static(
        "sandbox_static",
        SANDBOX_STATIC,
        Some(r#"<div class="greeting" id="root">hello world</div>"#),
    );
}

const SANDBOX_NESTED: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, span, text, ul, li)
import Html.Attributes exposing (class, style)

type Msg = NoOp

main : Program () Int Msg
main =
    Browser.sandbox { init = 0, update = \_ m -> m, view = view }

view : Int -> Html Msg
view _ =
    div [ class "outer", style "color" "red" ]
        [ span [] [ text "a & b < c" ]
        , ul []
            [ li [] [ text "one" ]
            , li [] [ text "two" ]
            ]
        ]
"#;

#[test]
fn sandbox_nested_and_escaping_matches_js() {
    assert_static("sandbox_nested", SANDBOX_NESTED, None);
}

// -- interactive tests -------------------------------------------------------

const COUNTER: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, button, div, text)
import Html.Attributes exposing (class)
import Html.Events exposing (onClick)

type Msg = Increment | Decrement

main : Program () Int Msg
main =
    Browser.sandbox { init = 0, update = update, view = view }

update : Msg -> Int -> Int
update msg model =
    case msg of
        Increment -> model + 1
        Decrement -> model - 1

view : Int -> Html Msg
view model =
    div [ class "counter" ]
        [ button [ class "dec", onClick Decrement ] [ text "-" ]
        , div [ class "value" ] [ text (String.fromInt model) ]
        , button [ class "inc", onClick Increment ] [ text "+" ]
        ]
"#;

#[test]
fn counter_click_updates_matches_js() {
    // button[0] = "-", button[1] = "+". Click +, +, -, then + again → 2.
    let steps = [
        step("button", 1, "click", "{}"),
        step("button", 1, "click", "{}"),
        step("button", 0, "click", "{}"),
        step("button", 1, "click", "{}"),
    ];
    assert_interaction(
        "counter",
        COUNTER,
        &steps,
        Some(
            r#"<div class="counter"><button class="dec">-</button><div class="value">2</div><button class="inc">+</button></div>"#,
        ),
    );
}

const TEXT_INPUT: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, input, text)
import Html.Attributes exposing (value)
import Html.Events exposing (onInput)

type Msg = Changed String

main : Program () String Msg
main =
    Browser.sandbox { init = "", update = update, view = view }

update : Msg -> String -> String
update (Changed s) _ = s

view : String -> Html Msg
view model =
    div []
        [ input [ value model, onInput Changed ] []
        , div [] [ text ("You typed: " ++ model) ]
        ]
"#;

#[test]
fn text_input_oninput_matches_js() {
    let steps = [
        step("input", 0, "input", r#"{"target":{"value":"hello"}}"#),
        step("input", 0, "input", r#"{"target":{"value":"hello world"}}"#),
    ];
    assert_interaction("text_input", TEXT_INPUT, &steps, None);
}

// Conditional structure: toggling adds/removes a subtree, exercising
// replace / append / remove-child in the patch.
const TOGGLE: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, button, div, p, text)
import Html.Events exposing (onClick)

type Msg = Toggle

main : Program () Bool Msg
main =
    Browser.sandbox { init = False, update = \_ m -> not m, view = view }

view : Bool -> Html Msg
view shown =
    div []
        (button [] [ text "toggle" ]
            :: (if shown then [ p [] [ text "now you see me" ] ] else [])
        )
"#;

#[test]
fn conditional_subtree_toggle_matches_js() {
    let steps = [
        step("button", 0, "click", "{}"),
        step("button", 0, "click", "{}"),
        step("button", 0, "click", "{}"),
    ];
    assert_interaction("toggle", TOGGLE, &steps, None);
}

// Attribute add/remove/change across an update (Phase 3 fact diff).
const ATTR_DIFF: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, button, div, text)
import Html.Attributes exposing (class, style, title, disabled)
import Html.Events exposing (onClick)

type Msg = Next

main : Program () Int Msg
main =
    Browser.sandbox { init = 0, update = \_ m -> m + 1, view = view }

view : Int -> Html Msg
view n =
    let
        attrs =
            case modBy 3 n of
                0 -> [ class "a", style "color" "red" ]
                1 -> [ class "b", title "hi", disabled True ]
                _ -> [ style "color" "blue" ]
    in
    div [] [ button (onClick Next :: attrs) [ text "x" ] ]
"#;

#[test]
fn attribute_add_remove_change_matches_js() {
    let steps = [
        step("button", 0, "click", "{}"),
        step("button", 0, "click", "{}"),
        step("button", 0, "click", "{}"),
        step("button", 0, "click", "{}"),
    ];
    assert_interaction("attr_diff", ATTR_DIFF, &steps, None);
}

// Keyed nodes reordering + insertion/removal (Phase 3 keyed reconciliation).
const KEYED: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, button, div, text)
import Html.Keyed as Keyed
import Html.Events exposing (onClick)

type Msg = Next

main : Program () Int Msg
main =
    Browser.sandbox { init = 0, update = \_ m -> m + 1, view = view }

row : String -> ( String, Html Msg )
row s = ( s, div [] [ text s ] )

view : Int -> Html Msg
view n =
    let
        items =
            case modBy 3 n of
                0 -> [ "a", "b", "c" ]
                1 -> [ "c", "a", "b", "d" ]
                _ -> [ "b", "d" ]
    in
    div []
        [ button [ onClick Next ] [ text "next" ]
        , Html.span [] [ text (String.fromInt n) ]
        , Keyed.node "div" [] (List.map row items)
        ]
"#;

#[test]
fn keyed_reorder_matches_js() {
    let steps = [
        step("button", 0, "click", "{}"),
        step("button", 0, "click", "{}"),
        step("button", 0, "click", "{}"),
    ];
    assert_interaction("keyed", KEYED, &steps, None);
}

// SVG namespaced element + attribute.
const SVG_NS: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html)
import Svg exposing (svg, circle)
import Svg.Attributes exposing (width, height, cx, cy, r, fill)

type Msg = NoOp

main : Program () Int Msg
main =
    Browser.sandbox { init = 0, update = \_ m -> m, view = view }

view : Int -> Html Msg
view _ =
    svg [ width "100", height "100" ]
        [ circle [ cx "50", cy "50", r "40", fill "green" ] [] ]
"#;

#[test]
fn svg_namespaced_matches_js() {
    assert_static("svg_ns", SVG_NS, None);
}

// -- Browser.element: effects -------------------------------------------------

const ELEMENT_TIMER: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, text)
import Time

type Msg = Tick Time.Posix

main : Program () Int Msg
main =
    Browser.element
        { init = \_ -> ( 0, Cmd.none )
        , update = update
        , view = view
        , subscriptions = \_ -> Time.every 1000 Tick
        }

update : Msg -> Int -> ( Int, Cmd Msg )
update _ n = ( n + 1, Cmd.none )

view : Int -> Html Msg
view n =
    div [] [ text ("ticks: " ++ String.fromInt n) ]
"#;

#[test]
fn element_time_every_matches_js() {
    // Advance across several interval boundaries; both backends must tick alike.
    let steps = [advance(2500), advance(1000), advance(5000)];
    assert_interaction("element_timer", ELEMENT_TIMER, &steps, None);
}

const ELEMENT_ANIM: &str = r#"
module Test exposing (main)

import Browser
import Browser.Events
import Html exposing (Html, div, text)
import Time

type Msg = Frame Time.Posix

main : Program () Int Msg
main =
    Browser.element
        { init = \_ -> ( 0, Cmd.none )
        , update = update
        , view = view
        , subscriptions = \_ -> Browser.Events.onAnimationFrame Frame
        }

update : Msg -> Int -> ( Int, Cmd Msg )
update (Frame t) _ = ( Time.posixToMillis t, Cmd.none )

view : Int -> Html Msg
view t =
    div [] [ text (String.fromInt t) ]
"#;

#[test]
fn element_animation_frame_matches_js() {
    // Absolute frame time (posix): a frame reports the current clock, so a
    // frame dispatched more than once with the same clock is idempotent —
    // sidestepping the JS runtime's rAF-reregistration delta quirk.
    let steps = [advance(16), frame(), advance(17), frame(), advance(16), frame()];
    assert_interaction("element_anim", ELEMENT_ANIM, &steps, None);
}

const ELEMENT_KEYS: &str = r#"
module Test exposing (main)

import Browser
import Browser.Events
import Html exposing (Html, div, text)
import Json.Decode as D

type Msg = Key String

main : Program () String Msg
main =
    Browser.element
        { init = \_ -> ( "none", Cmd.none )
        , update = \(Key k) _ -> ( k, Cmd.none )
        , view = view
        , subscriptions = \_ -> Browser.Events.onKeyDown (D.map Key (D.field "key" D.string))
        }

view : String -> Html Msg
view k =
    div [] [ text ("key: " ++ k) ]
"#;

#[test]
fn element_keyboard_subscription_matches_js() {
    let steps = [
        doc_event("keydown", r#"{"key":"a"}"#),
        doc_event("keydown", r#"{"key":"Enter"}"#),
    ];
    assert_interaction("element_keys", ELEMENT_KEYS, &steps, None);
}

// -- Browser.document ---------------------------------------------------------

const DOCUMENT: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, text, button)
import Html.Events exposing (onClick)

type Msg = Bump

main : Program () Int Msg
main =
    Browser.document
        { init = \_ -> ( 0, Cmd.none )
        , update = \_ n -> ( n + 1, Cmd.none )
        , view = view
        , subscriptions = \_ -> Sub.none
        }

view : Int -> Browser.Document Msg
view n =
    { title = "Count " ++ String.fromInt n
    , body = [ button [ onClick Bump ] [ text "+" ], div [] [ text (String.fromInt n) ] ]
    }
"#;

#[test]
fn document_body_and_updates_match_js() {
    let steps = [step("button", 0, "click", "{}"), step("button", 0, "click", "{}")];
    assert_interaction("document", DOCUMENT, &steps, None);
}

// -- async Task (Process.sleep) ----------------------------------------------

const SLEEP_TASK: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, text)
import Process
import Task

type Msg = Done

main : Program () Bool Msg
main =
    Browser.element
        { init = \_ -> ( False, Task.perform (\_ -> Done) (Process.sleep 1000) )
        , update = \_ _ -> ( True, Cmd.none )
        , view = \done -> div [] [ text (if done then "done" else "waiting") ]
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn process_sleep_task_matches_js() {
    // Before the timer fires: "waiting"; after advancing past it: "done".
    let steps = [advance(500), advance(600)];
    assert_interaction("sleep_task", SLEEP_TASK, &steps, None);
}

// -- ports (Json.Value payloads) ---------------------------------------------

const PORTS: &str = r#"
port module Test exposing (main)

import Browser
import Html exposing (Html, div, text, button)
import Html.Events exposing (onClick)
import Json.Encode as E
import Json.Decode as D

port toJs : E.Value -> Cmd msg
port fromJs : (D.Value -> msg) -> Sub msg

type Msg = Send | Got D.Value

main : Program () Int Msg
main =
    Browser.element
        { init = \_ -> ( 0, Cmd.none )
        , update = update
        , view = view
        , subscriptions = \_ -> fromJs Got
        }

update : Msg -> Int -> ( Int, Cmd Msg )
update msg n =
    case msg of
        Send -> ( n, toJs (E.int n) )
        Got _ -> ( n + 1, Cmd.none )

view : Int -> Html Msg
view n =
    div [] [ button [ onClick Send ] [ text "send" ], div [] [ text (String.fromInt n) ] ]
"#;

#[test]
fn ports_roundtrip_matches_js() {
    let steps = [
        port("fromJs", "{}"),               // incoming Got -> n = 1
        port("fromJs", "42"),               // incoming Got -> n = 2
        step("button", 0, "click", "{}"),   // Send -> outgoing toJs (E.int 2)
    ];
    assert_interaction("ports", PORTS, &steps, None);
}

// -- Browser.application (URL + navigation) ----------------------------------

const APPLICATION: &str = r#"
module Test exposing (main)

import Browser
import Browser.Navigation as Nav
import Html exposing (Html, div, text, button)
import Html.Events exposing (onClick)
import Url exposing (Url)

type alias Model = { key : Nav.Key, path : String }
type Msg = Go | Changed Url | Clicked Browser.UrlRequest

main : Program () Model Msg
main =
    Browser.application
        { init = init
        , update = update
        , view = view
        , subscriptions = \_ -> Sub.none
        , onUrlRequest = Clicked
        , onUrlChange = Changed
        }

init : () -> Url -> Nav.Key -> ( Model, Cmd Msg )
init _ url key = ( { key = key, path = url.path }, Cmd.none )

update : Msg -> Model -> ( Model, Cmd Msg )
update msg model =
    case msg of
        Go -> ( model, Nav.pushUrl model.key "/two" )
        Changed url -> ( { model | path = url.path }, Cmd.none )
        Clicked _ -> ( model, Cmd.none )

view : Model -> Browser.Document Msg
view model =
    { title = "app"
    , body = [ button [ onClick Go ] [ text "go" ], div [] [ text model.path ] ]
    }
"#;

#[test]
fn application_url_and_pushurl_matches_js() {
    // Initial path "/", then clicking pushes "/two" and onUrlChange updates it.
    let steps = [step("button", 0, "click", "{}")];
    assert_interaction("application", APPLICATION, &steps, None);
}

// -- Html.Lazy (forced eagerly on the native side) ---------------------------

const LAZY: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, text, button)
import Html.Lazy exposing (lazy)
import Html.Events exposing (onClick)

type Msg = Inc

main : Program () Int Msg
main = Browser.sandbox { init = 0, update = \_ n -> n + 1, view = view }

viewCount : Int -> Html Msg
viewCount n = div [] [ text (String.fromInt n) ]

view : Int -> Html Msg
view n =
    div [] [ button [ onClick Inc ] [ text "+" ], lazy viewCount n ]
"#;

#[test]
fn html_lazy_matches_js() {
    let steps = [step("button", 0, "click", "{}"), step("button", 0, "click", "{}")];
    assert_interaction("lazy", LAZY, &steps, None);
}

// -- HTTP --------------------------------------------------------------------

const HTTP_STRING: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, text, button)
import Html.Events exposing (onClick)
import Http

type Msg = Fetch | Got (Result Http.Error String)

main : Program () String Msg
main =
    Browser.element
        { init = \_ -> ( "start", Cmd.none )
        , update = update
        , view = view
        , subscriptions = \_ -> Sub.none
        }

update : Msg -> String -> ( String, Cmd Msg )
update msg model =
    case msg of
        Fetch -> ( model, Http.get { url = "/data", expect = Http.expectString Got } )
        Got (Ok s) -> ( "ok:" ++ s, Cmd.none )
        Got (Err (Http.BadStatus code)) -> ( "bad:" ++ String.fromInt code, Cmd.none )
        Got (Err _) -> ( "err", Cmd.none )

view : String -> Html Msg
view model =
    div [] [ button [ onClick Fetch ] [ text "fetch" ], div [] [ text model ] ]
"#;

#[test]
fn http_expect_string_matches_js() {
    let steps = [
        step("button", 0, "click", "{}"),
        http(200, "hello"),
        step("button", 0, "click", "{}"),
        http(404, "nope"),
        step("button", 0, "click", "{}"),
        http(0, ""), // network error
    ];
    assert_interaction("http_string", HTTP_STRING, &steps, None);
}

const HTTP_JSON: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, text, button)
import Html.Events exposing (onClick)
import Http
import Json.Decode as D

type Msg = Fetch | Got (Result Http.Error Int)

main : Program () String Msg
main =
    Browser.element
        { init = \_ -> ( "start", Cmd.none )
        , update = update
        , view = view
        , subscriptions = \_ -> Sub.none
        }

update : Msg -> String -> ( String, Cmd Msg )
update msg model =
    case msg of
        Fetch -> ( model, Http.get { url = "/n", expect = Http.expectJson Got (D.field "n" D.int) } )
        Got (Ok n) -> ( "n=" ++ String.fromInt n, Cmd.none )
        Got (Err _) -> ( "err", Cmd.none )

view : String -> Html Msg
view model =
    div [] [ button [ onClick Fetch ] [ text "fetch" ], div [] [ text model ] ]
"#;

#[test]
fn http_expect_json_matches_js() {
    let steps = [
        step("button", 0, "click", "{}"),
        http(200, r#"{"n":42}"#),
        step("button", 0, "click", "{}"),
        http(200, r#"{"bad":1}"#), // decode failure -> BadBody -> "err"
    ];
    assert_interaction("http_json", HTTP_JSON, &steps, None);
}

// -- flags (Json.Value) ------------------------------------------------------

const FLAGS: &str = r#"
module Test exposing (main)

import Browser
import Html exposing (Html, div, text)
import Json.Decode as D

type Msg = NoOp

main : Program D.Value String Msg
main =
    Browser.element
        { init = init
        , update = \_ m -> ( m, Cmd.none )
        , view = \m -> div [] [ text m ]
        , subscriptions = \_ -> Sub.none
        }

init : D.Value -> ( String, Cmd Msg )
init flags =
    ( Result.withDefault "?" (D.decodeValue (D.field "name" D.string) flags), Cmd.none )
"#;

#[test]
fn flags_json_value_matches_js() {
    assert_interaction_flags(
        "flags",
        FLAGS,
        &[],
        Some(r#"<div>Ada</div>"#),
        r#"{"name":"Ada"}"#,
    );
}
