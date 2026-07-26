module Main exposing (main)

import Browser
import Html exposing (Html)
import Html.Attributes exposing (height, id, width)
import Math.Vector2 exposing (Vec2, vec2)
import Task
import WebGL exposing (Mesh, Shader)
import WebGL.Texture as Texture exposing (Texture)


-- A 2x2 fully-opaque green PNG. Texture.load fetches it (new Image()) and the
-- renderer uploads it to a GL texture on first draw; the fragment shader samples
-- it, so the canvas must come out green.
greenTexture : String
greenTexture =
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEklEQVR4AWJiOMHwH4SZGKAAAAAA//8+GSofAAAABklEQVQDAC4QA5OogoEuAAAAAElFTkSuQmCC"


type alias Vertex =
    { position : Vec2 }


type alias Model =
    { tex : Maybe Texture }


type Msg
    = Loaded (Result Texture.Error Texture)


mesh : Mesh Vertex
mesh =
    WebGL.triangleStrip
        [ { position = vec2 -1 -1 }
        , { position = vec2 1 -1 }
        , { position = vec2 -1 1 }
        , { position = vec2 1 1 }
        ]


type alias Uniforms =
    { tex : Texture }


vert : Shader Vertex Uniforms { vuv : Vec2 }
vert =
    [glsl|
        attribute vec2 position;
        varying vec2 vuv;
        void main () {
            vuv = position * 0.5 + 0.5;
            gl_Position = vec4(position, 0.0, 1.0);
        }
    |]


frag : Shader {} Uniforms { vuv : Vec2 }
frag =
    [glsl|
        precision mediump float;
        uniform sampler2D tex;
        varying vec2 vuv;
        void main () { gl_FragColor = texture2D(tex, vuv); }
    |]


view : Model -> Html Msg
view model =
    case model.tex of
        Nothing ->
            Html.text "loading"

        Just t ->
            WebGL.toHtmlWith
                [ WebGL.preserveDrawingBuffer, WebGL.clearColor 0 0 1 1 ]
                [ width 100, height 100, id "glcanvas" ]
                [ WebGL.entity vert frag mesh { tex = t } ]


main : Program () Model Msg
main =
    Browser.element
        { init = \_ -> ( { tex = Nothing }, Task.attempt Loaded (Texture.load greenTexture) )
        , update =
            \msg _ ->
                case msg of
                    Loaded (Ok t) ->
                        ( { tex = Just t }, Cmd.none )

                    Loaded (Err _) ->
                        ( { tex = Nothing }, Cmd.none )
        , view = view
        , subscriptions = \_ -> Sub.none
        }
