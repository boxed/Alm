module Main exposing (bad)


greet : String -> String
greet name =
    "hi " ++ name


bad : String
bad =
    greet 5
