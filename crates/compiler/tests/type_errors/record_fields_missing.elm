module Main exposing (bad)


needs : { x : Int, y : Int, z : Int } -> Int
needs r =
    r.x


bad : Int
bad =
    needs { x = 1 }
