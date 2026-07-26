module Main exposing (bad)


point : { name : String }
point =
    { name = "x" }


bad : String
bad =
    point.nmae
