port module Main exposing (main)

{-| Browser runtime stress test. Compiled with both alm and elm 0.19.1;
the same in-page harness drives both and the results must agree.
-}

import Browser
import Html exposing (Html, button, div, form, input, li, span, text, ul)
import Html.Attributes exposing (checked, class, disabled, id, style, type_, value)
import Html.Events exposing (custom, onCheck, onClick, onInput, onSubmit, stopPropagationOn)
import Html.Keyed as Keyed
import Html.Lazy
import Json.Decode as Decode
import Process
import Svg
import Svg.Attributes as SvgAttr
import Task


port fromJs : (String -> msg) -> Sub msg


port toJs : String -> Cmd msg


type alias Item =
    { key : String, label : String }


type alias Model =
    { count : Int
    , items : List Item
    , textValue : String
    , checkboxOn : Bool
    , submitted : Int
    , outerClicks : Int
    , innerClicks : Int
    , customLog : List String
    , slept : Bool
    , portEcho : String
    , showPanel : Bool
    , styleToggle : Bool
    }


type Msg
    = Increment
    | ChildIncrement ChildMsg
    | Reorder
    | InsertFront
    | RemoveSecond
    | TextChanged String
    | CheckboxChanged Bool
    | FormSubmitted
    | OuterClicked
    | InnerClicked
    | CustomEvent String
    | Slept ()
    | GotFromJs String
    | TogglePanel
    | ToggleStyle


type ChildMsg
    = Poke


init : () -> ( Model, Cmd Msg )
init _ =
    ( { count = 0
      , items =
            [ Item "a" "alpha"
            , Item "b" "beta"
            , Item "c" "gamma"
            ]
      , textValue = ""
      , checkboxOn = False
      , submitted = 0
      , outerClicks = 0
      , innerClicks = 0
      , customLog = []
      , slept = False
      , portEcho = ""
      , showPanel = False
      , styleToggle = False
      }
    , Task.perform Slept (Process.sleep 30)
    )


update : Msg -> Model -> ( Model, Cmd Msg )
update msg model =
    case msg of
        Increment ->
            ( { model | count = model.count + 1 }, Cmd.none )

        ChildIncrement Poke ->
            ( { model | count = model.count + 10 }, Cmd.none )

        Reorder ->
            ( { model | items = List.reverse model.items }, Cmd.none )

        InsertFront ->
            ( { model | items = Item "new" "newcomer" :: model.items }, Cmd.none )

        RemoveSecond ->
            ( { model
                | items =
                    List.take 1 model.items ++ List.drop 2 model.items
              }
            , Cmd.none
            )

        TextChanged s ->
            ( { model | textValue = s }, Cmd.none )

        CheckboxChanged b ->
            ( { model | checkboxOn = b }, Cmd.none )

        FormSubmitted ->
            ( { model | submitted = model.submitted + 1 }, Cmd.none )

        OuterClicked ->
            ( { model | outerClicks = model.outerClicks + 1 }, Cmd.none )

        InnerClicked ->
            ( { model | innerClicks = model.innerClicks + 1 }, Cmd.none )

        CustomEvent tag ->
            ( { model | customLog = tag :: model.customLog }, Cmd.none )

        Slept () ->
            ( { model | slept = True }, Cmd.none )

        GotFromJs s ->
            ( { model | portEcho = s }, toJs ("echo:" ++ s) )

        TogglePanel ->
            ( { model | showPanel = not model.showPanel }, Cmd.none )

        ToggleStyle ->
            ( { model | styleToggle = not model.styleToggle }, Cmd.none )


subscriptions : Model -> Sub Msg
subscriptions _ =
    fromJs GotFromJs


viewItem : Item -> Html Msg
viewItem item =
    li [ class "item" ] [ text item.label ]


viewBadge : Int -> Html Msg
viewBadge n =
    span [ id "lazy-badge" ] [ text ("badge:" ++ String.fromInt n) ]


childView : Html ChildMsg
childView =
    button [ id "child-button", onClick Poke ] [ text "poke" ]


view : Model -> Html Msg
view model =
    div [ id "app-root" ]
        [ div [ id "count" ] [ text (String.fromInt model.count) ]
        , button [ id "inc", onClick Increment ] [ text "+" ]
        , Html.map ChildIncrement childView
        , button [ id "reorder", onClick Reorder ] [ text "reorder" ]
        , button [ id "insert", onClick InsertFront ] [ text "insert" ]
        , button [ id "remove", onClick RemoveSecond ] [ text "remove" ]
        , Keyed.ul [ id "keyed-list" ]
            (List.map (\item -> ( item.key, viewItem item )) model.items)
        , input [ id "text-in", type_ "text", value model.textValue, onInput TextChanged ] []
        , div [ id "text-out" ] [ text (String.reverse model.textValue) ]
        , input [ id "check-in", type_ "checkbox", checked model.checkboxOn, onCheck CheckboxChanged ] []
        , div [ id "check-out" ]
            [ text
                (if model.checkboxOn then
                    "on"

                 else
                    "off"
                )
            ]
        , form [ id "the-form", onSubmit FormSubmitted ]
            [ button [ id "submit-btn", type_ "submit" ] [ text "go" ] ]
        , div [ id "submit-out" ] [ text (String.fromInt model.submitted) ]
        , div [ id "outer", onClick OuterClicked ]
            [ button
                [ id "stopper"
                , stopPropagationOn "click" (Decode.succeed ( InnerClicked, True ))
                ]
                [ text "stop" ]
            , button [ id "bubbler", onClick InnerClicked ] [ text "bubble" ]
            ]
        , div [ id "click-out" ]
            [ text (String.fromInt model.outerClicks ++ "/" ++ String.fromInt model.innerClicks) ]
        , button
            [ id "custom-btn"
            , custom "click"
                (Decode.succeed
                    { message = CustomEvent "custom"
                    , stopPropagation = True
                    , preventDefault = True
                    }
                )
            ]
            [ text "custom" ]
        , div [ id "custom-out" ] [ text (String.join "," model.customLog) ]
        , div [ id "sleep-out" ]
            [ text
                (if model.slept then
                    "awake"

                 else
                    "sleeping"
                )
            ]
        , div [ id "port-out" ] [ text model.portEcho ]
        , Html.Lazy.lazy viewBadge model.count
        , if model.showPanel then
            div [ id "panel" ] [ text "panel-content" ]

          else
            text ""
        , button [ id "toggle-panel", onClick TogglePanel ] [ text "toggle" ]
        , div
            [ id "styled"
            , style "color"
                (if model.styleToggle then
                    "rgb(255, 0, 0)"

                 else
                    "rgb(0, 0, 255)"
                )
            , class
                (if model.styleToggle then
                    "hot"

                 else
                    "cold"
                )
            , disabled model.styleToggle
            ]
            [ text "styled" ]
        , button [ id "toggle-style", onClick ToggleStyle ] [ text "style" ]
        , Svg.svg [ SvgAttr.viewBox "0 0 100 100", SvgAttr.width "50", id "the-svg" ]
            [ Svg.circle [ SvgAttr.cx "50", SvgAttr.cy "50", SvgAttr.r "40", SvgAttr.fill "green" ] [] ]
        ]


main : Program () Model Msg
main =
    Browser.element
        { init = init
        , update = update
        , subscriptions = subscriptions
        , view = view
        }
