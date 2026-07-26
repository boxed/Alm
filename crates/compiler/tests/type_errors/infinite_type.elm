module Main exposing (bad)


bad : a
bad =
    let
        f x =
            x x
    in
    f
