module Main exposing (bad)


bad : Maybe Int -> Int
bad m =
    case m of
        Just n ->
            n

        Nothing ->
            "none"
