module Main exposing (bad)


bad : { a : { b : Int } } -> String
bad r =
    r.a.b
