module Main exposing (main)

import Browser
import Html exposing (..)
import Html.Attributes exposing (class, id)
import Html.Events exposing (onClick)
import Html.Keyed as Keyed


type alias Row = { id : Int, label : String }
type alias Model = { rows : List Row, selected : Int, next : Int }


label : Int -> String
label i = "item " ++ String.fromInt i


mk : Int -> Int -> List Row
mk start n =
    List.map (\i -> { id = i, label = label i }) (List.range start (start + n - 1))


type Msg
    = Create Int
    | Append Int
    | UpdateEvery
    | Swap
    | Select Int
    | Remove Int
    | Clear


init : () -> ( Model, Cmd Msg )
init _ = ( { rows = [], selected = 0, next = 1 }, Cmd.none )


update : Msg -> Model -> ( Model, Cmd Msg )
update msg m =
    case msg of
        Create n -> ( { m | rows = mk m.next n, next = m.next + n }, Cmd.none )
        Append n -> ( { m | rows = m.rows ++ mk m.next n, next = m.next + n }, Cmd.none )
        UpdateEvery ->
            ( { m | rows = List.indexedMap (\i r -> if modBy 10 i == 0 then { r | label = r.label ++ " !!!" } else r) m.rows }, Cmd.none )
        Swap ->
            case m.rows of
                a :: rest ->
                    case List.reverse rest of
                        z :: mid -> ( { m | rows = z :: List.reverse mid ++ [ a ] }, Cmd.none )
                        [] -> ( m, Cmd.none )
                [] -> ( m, Cmd.none )
        Select i -> ( { m | selected = i }, Cmd.none )
        Remove i -> ( { m | rows = List.filter (\r -> r.id /= i) m.rows }, Cmd.none )
        Clear -> ( { m | rows = [], selected = 0 }, Cmd.none )


viewRow : Int -> Row -> ( String, Html Msg )
viewRow sel r =
    ( String.fromInt r.id
    , tr [ class (if r.id == sel then "danger" else "") ]
        [ td [ class "col-md-1" ] [ text (String.fromInt r.id) ]
        , td [ class "col-md-4" ] [ a [ onClick (Select r.id) ] [ text r.label ] ]
        , td [ class "col-md-1" ] [ a [ onClick (Remove r.id) ] [ span [ class "glyphicon glyphicon-remove" ] [] ] ]
        , td [ class "col-md-6" ] []
        ]
    )


view : Model -> Html Msg
view m =
    div [ id "main" ]
        [ button [ id "create", onClick (Create 1000) ] [ text "create" ]
        , button [ id "create10k", onClick (Create 10000) ] [ text "create10k" ]
        , button [ id "append", onClick (Append 1000) ] [ text "append" ]
        , button [ id "update", onClick UpdateEvery ] [ text "update" ]
        , button [ id "swap", onClick Swap ] [ text "swap" ]
        , button [ id "clear", onClick Clear ] [ text "clear" ]
        , Keyed.node "table" [ class "table" ] (List.map (viewRow m.selected) m.rows)
        ]


main : Program () Model Msg
main = Browser.element { init = init, update = update, view = view, subscriptions = \_ -> Sub.none }
