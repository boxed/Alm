module Main exposing (main)

import Browser
import Html exposing (Html)
import Html.Attributes exposing (height, id, width)
import Math.Matrix4 as Mat4 exposing (Mat4)
import Math.Vector3 exposing (Vec3, vec3)
import WebGL exposing (Mesh, Shader)


type alias Vertex = { position : Vec3, color : Vec3 }


mesh : Mesh Vertex
mesh =
    WebGL.indexedTriangles
        [ { position = vec3 -0.8 -0.8 0, color = vec3 0 1 0 }
        , { position = vec3 0.8 -0.8 0, color = vec3 0 1 0 }
        , { position = vec3 0.8 0.8 0, color = vec3 0 1 0 }
        , { position = vec3 -0.8 0.8 0, color = vec3 0 1 0 }
        ]
        [ ( 0, 1, 2 ), ( 0, 2, 3 ) ]


type alias Uniforms = { transform : Mat4 }


vertexShader : Shader Vertex Uniforms { vcolor : Vec3 }
vertexShader =
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


fragmentShader : Shader {} Uniforms { vcolor : Vec3 }
fragmentShader =
    [glsl|
        precision mediump float;
        varying vec3 vcolor;
        void main () { gl_FragColor = vec4(vcolor, 1.0); }
    |]


view : () -> Html ()
view _ =
    WebGL.toHtmlWith
        [ WebGL.preserveDrawingBuffer, WebGL.clearColor 0 0 1 1 ]
        [ width 100, height 100, id "glcanvas" ]
        [ WebGL.entity vertexShader fragmentShader mesh { transform = Mat4.makeScale (vec3 0.5 0.5 1) } ]


main : Program () () ()
main =
    Browser.element { init = \_ -> ( (), Cmd.none ), update = \_ m -> ( m, Cmd.none ), view = view, subscriptions = \_ -> Sub.none }
