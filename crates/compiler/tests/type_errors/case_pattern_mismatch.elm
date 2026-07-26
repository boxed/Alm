module Main exposing (bad)


bad : Int -> Int
bad n =
    case n of
        "x" ->
            1

        _ ->
            2
