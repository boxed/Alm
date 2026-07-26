module Main exposing (bad)


bad : Bool
bad =
    (\x -> x) < identity
