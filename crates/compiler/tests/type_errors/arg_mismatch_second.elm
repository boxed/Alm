module Main exposing (bad)


pair : String -> Int -> String
pair a b =
    a ++ String.fromInt b


bad : String
bad =
    pair "x" "y"
