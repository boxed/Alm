module Main exposing (bad)


point : { x : Int, y : Int }
point =
    { x = 1, y = 2 }


bad : { x : Int, y : Int }
bad =
    { point | x = "one" }
