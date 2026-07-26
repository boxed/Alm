//! WebGL (elm-explorations/webgl) compiles and wires up the real rendering
//! kernel — not the old inert stub. This runs in `cargo test` without a browser:
//! it checks the front end accepts a WebGL program (GLSL shaders, meshes,
//! Math.Vector/Matrix uniforms) and that the generated JS contains the actual
//! rendering pipeline (`_WebGL_drawGL`, `gl.drawArrays`, the custom vdom widget).
//! Real GPU pixel output is verified separately in tests/browser/webgl (headless
//! Chromium), since node has no WebGL context.

mod common;

use alm_compiler::{generate, project};

const ELM_JSON: &str = r#"{
    "type": "application",
    "source-directories": ["src"],
    "elm-version": "0.19.1",
    "dependencies": {
        "direct": {
            "elm/browser": "1.0.2",
            "elm/core": "1.0.5",
            "elm/html": "1.0.0",
            "elm/json": "1.1.3",
            "elm-explorations/linear-algebra": "1.0.3",
            "elm-explorations/webgl": "1.1.3"
        },
        "indirect": {
            "elm/time": "1.0.0",
            "elm/url": "1.0.0",
            "elm/virtual-dom": "1.0.3"
        }
    },
    "test-dependencies": { "direct": {}, "indirect": {} }
}"#;

const MAIN: &str = r#"module Main exposing (main)

import Browser
import Html exposing (Html)
import Html.Attributes exposing (height, width)
import Math.Matrix4 as Mat4 exposing (Mat4)
import Math.Vector3 exposing (Vec3, vec3)
import WebGL exposing (Mesh, Shader)


type alias Vertex = { position : Vec3, color : Vec3 }


mesh : Mesh Vertex
mesh =
    WebGL.indexedTriangles
        [ { position = vec3 -1 -1 0, color = vec3 1 0 0 }
        , { position = vec3 1 -1 0, color = vec3 0 1 0 }
        , { position = vec3 0 1 0, color = vec3 0 0 1 }
        ]
        [ ( 0, 1, 2 ) ]


type alias Uniforms = { transform : Mat4 }


vert : Shader Vertex Uniforms { vcolor : Vec3 }
vert =
    [glsl|
        attribute vec3 position;
        attribute vec3 color;
        uniform mat4 transform;
        varying vec3 vcolor;
        void main () {
            gl_Position = transform * vec4(position, 1.0);
            vcolor = color;
        }
    |]


frag : Shader {} Uniforms { vcolor : Vec3 }
frag =
    [glsl|
        precision mediump float;
        varying vec3 vcolor;
        void main () { gl_FragColor = vec4(vcolor, 1.0); }
    |]


view : () -> Html ()
view _ =
    WebGL.toHtmlWith
        [ WebGL.preserveDrawingBuffer, WebGL.clearColor 0 0 1 1, WebGL.depth 1 ]
        [ width 100, height 100 ]
        [ WebGL.entity vert frag mesh { transform = Mat4.identity } ]


main : Program () () ()
main =
    Browser.element
        { init = \_ -> ( (), Cmd.none )
        , update = \_ m -> ( m, Cmd.none )
        , view = view
        , subscriptions = \_ -> Sub.none
        }
"#;

#[test]
fn webgl_program_compiles_with_real_kernel() {
    let dir = common::test_dir("alm-webgl", "prog");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), MAIN).unwrap();

    let checked = match project::check_project(&src.join("Main.elm")) {
        Ok(c) => c,
        Err(errors) => {
            // elm-explorations/webgl not in the local ~/.elm cache — skip rather
            // than fail (the render path is covered by tests/browser/webgl).
            let msg = errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n");
            if msg.contains("webgl") || msg.contains("linear-algebra") || msg.contains("find") {
                eprintln!("skipping webgl_test: package unavailable\n{msg}");
                return;
            }
            panic!("WebGL program failed to compile:\n{msg}");
        }
    };

    let js = generate::generate_project(&checked.modules);

    // The real rendering kernel must be present and reachable (not tree-shaken,
    // not the old inert stub).
    for needle in [
        "_WebGL_drawGL",           // the draw loop
        "gl.drawElements",         // indexed draw
        "gl.drawArrays",           // non-indexed draw
        "_WebGL_doLink",           // shader program linking
        "_VirtualDom_custom",      // the canvas widget node
        "uniformMatrix4fv",        // mat4 uniform upload
        "$WebGL$Internal$enableSetting", // settings dispatch reachable
    ] {
        assert!(js.contains(needle), "generated JS is missing `{needle}` — WebGL kernel not wired");
    }
    // The old inert stub must be gone.
    assert!(!js.contains("WebGLScene"), "found the old WebGL stub node `WebGLScene`");
}

const MAIN_TEX: &str = r#"module Main exposing (main)

import Browser
import Html exposing (Html)
import Math.Vector2 exposing (Vec2, vec2)
import Task
import WebGL exposing (Mesh, Shader)
import WebGL.Texture as Texture exposing (Texture)


type alias Vertex = { position : Vec2 }
type alias Model = { tex : Maybe Texture }
type Msg = Loaded (Result Texture.Error Texture)


mesh : Mesh Vertex
mesh =
    WebGL.triangleStrip [ { position = vec2 -1 -1 }, { position = vec2 1 1 } ]


vert : Shader Vertex { tex : Texture } { vuv : Vec2 }
vert =
    [glsl| attribute vec2 position; varying vec2 vuv;
           void main () { vuv = position; gl_Position = vec4(position, 0.0, 1.0); } |]


frag : Shader {} { tex : Texture } { vuv : Vec2 }
frag =
    [glsl| precision mediump float; uniform sampler2D tex; varying vec2 vuv;
           void main () { gl_FragColor = texture2D(tex, vuv); } |]


view : Model -> Html Msg
view model =
    case model.tex of
        Nothing -> Html.text "loading"
        Just t -> WebGL.toHtml [] [ WebGL.entity vert frag mesh { tex = t } ]


main : Program () Model Msg
main =
    Browser.element
        { init = \_ -> ( { tex = Nothing }, Task.attempt Loaded (Texture.load "x.png") )
        , update = \msg _ ->
            case msg of
                Loaded (Ok t) -> ( { tex = Just t }, Cmd.none )
                Loaded (Err _) -> ( { tex = Nothing }, Cmd.none )
        , view = view
        , subscriptions = \_ -> Sub.none
        }
"#;

/// A program that `Texture.load`s an image and samples it through a `sampler2D`
/// uniform must reach the real texture kernel — the async image load, the GPU
/// upload, and the SizeError/LoadError constructors (kept past DCE). Real pixel
/// output is covered by tests/browser/webgl-texture.
#[test]
fn webgl_texture_compiles_with_real_kernel() {
    let dir = common::test_dir("alm-webgl", "texture");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(dir.join("elm.json"), ELM_JSON).unwrap();
    std::fs::write(src.join("Main.elm"), MAIN_TEX).unwrap();

    let checked = match project::check_project(&src.join("Main.elm")) {
        Ok(c) => c,
        Err(errors) => {
            let msg = errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n");
            if msg.contains("webgl") || msg.contains("linear-algebra") || msg.contains("find") {
                eprintln!("skipping webgl_texture test: package unavailable\n{msg}");
                return;
            }
            panic!("WebGL texture program failed to compile:\n{msg}");
        }
    };

    let js = generate::generate_project(&checked.modules);

    for needle in [
        "gl.texImage2D",            // image upload to GPU
        "new Image()",              // async image fetch
        "generateMipmap",           // mipmap path
        "$WebGL$Texture$SizeError", // error ctors reachable (kept past DCE)
        "$WebGL$Texture$LoadError",
        "createTexture",            // per-texture upload closure
    ] {
        assert!(js.contains(needle), "generated JS is missing `{needle}` — texture kernel not wired");
    }
    // The old always-failing stub must be gone.
    assert!(!js.contains("err(_Utils_Tuple0); });"), "found the old texture stub `load`");
}
