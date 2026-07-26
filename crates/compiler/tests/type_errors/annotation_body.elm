module Main exposing (bad)


greet : String -> String
greet name =
    "hi " ++ name


bad : Int
bad =
    greet "x"
