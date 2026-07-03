module App exposing (main)

{-| Browser.application test: routing, link interception, pushUrl,
history back, and document titles. Compiled with both alm and elm.
-}

import Browser
import Browser.Navigation as Nav
import Html exposing (Html, a, button, div, text)
import Html.Attributes exposing (href, id)
import Html.Events exposing (onClick)
import Url


type alias Model =
    { key : Nav.Key
    , path : String
    , changes : Int
    }


type Msg
    = UrlRequested Browser.UrlRequest
    | UrlChanged Url.Url
    | GoThree


init : () -> Url.Url -> Nav.Key -> ( Model, Cmd Msg )
init _ url key =
    ( { key = key, path = url.path, changes = 0 }, Cmd.none )


update : Msg -> Model -> ( Model, Cmd Msg )
update msg model =
    case msg of
        UrlRequested (Browser.Internal url) ->
            ( model, Nav.pushUrl model.key url.path )

        UrlRequested (Browser.External href) ->
            ( model, Nav.load href )

        UrlChanged url ->
            ( { model | path = url.path, changes = model.changes + 1 }, Cmd.none )

        GoThree ->
            ( model, Nav.pushUrl model.key "/three" )


view : Model -> Browser.Document Msg
view model =
    { title = "page:" ++ model.path
    , body =
        [ div [ id "path" ] [ text model.path ]
        , div [ id "changes" ] [ text (String.fromInt model.changes) ]
        , a [ id "link-two", href "/two" ] [ text "to two" ]
        , button [ id "go-three", onClick GoThree ] [ text "to three" ]
        ]
    }


main : Program () Model Msg
main =
    Browser.application
        { init = init
        , update = update
        , subscriptions = \_ -> Sub.none
        , view = view
        , onUrlRequest = UrlRequested
        , onUrlChange = UrlChanged
        }
