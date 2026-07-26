module Main exposing (bad)


greet : String -> String
greet name =
    name


bad : String
bad =
    greet "a" "b"
