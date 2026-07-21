module Main_lazy exposing (main)

-- Same app as Main.elm but with Html.Lazy on the rows: each row is memoized on
-- (isSelected, row), both reference-stable across a select, so selecting a row
-- rebuilds/diffs only the two rows whose selection flipped (O(changed)) instead
-- of the whole list. This is the idiomatic vdom answer to Svelte's fine-grained
-- reactivity — see report.html's note on the `select` row.

import Browser
import Html exposing (..)
import Html.Attributes exposing (class, id)
import Html.Events exposing (onClick)
import Html.Keyed as Keyed
import Html.Lazy exposing (lazy2)


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


-- The row view depends only on whether it is selected (a Bool) and the row —
-- not the global `selected` int — so lazy2 memoizes it across a select.
viewRow : Bool -> Row -> Html Msg
viewRow selected r =
    tr [ class (if selected then "danger" else "") ]
        [ td [ class "col-md-1" ] [ text (String.fromInt r.id) ]
        , td [ class "col-md-4" ] [ a [ onClick (Select r.id) ] [ text r.label ] ]
        , td [ class "col-md-1" ] [ a [ onClick (Remove r.id) ] [ span [ class "glyphicon glyphicon-remove" ] [] ] ]
        , td [ class "col-md-6" ] []
        ]


view : Model -> Html Msg
view m =
    div [ id "main" ]
        [ button [ id "create", onClick (Create 1000) ] [ text "create" ]
        , button [ id "create10k", onClick (Create 10000) ] [ text "create10k" ]
        , button [ id "append", onClick (Append 1000) ] [ text "append" ]
        , button [ id "update", onClick UpdateEvery ] [ text "update" ]
        , button [ id "swap", onClick Swap ] [ text "swap" ]
        , button [ id "clear", onClick Clear ] [ text "clear" ]
        , Keyed.node "table" [ class "table" ]
            (List.map (\r -> ( String.fromInt r.id, lazy2 viewRow (r.id == m.selected) r )) m.rows)
        ]


main : Program () Model Msg
main = Browser.element { init = init, update = update, view = view, subscriptions = \_ -> Sub.none }
