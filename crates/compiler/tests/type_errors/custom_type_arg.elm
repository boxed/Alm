module Main exposing (bad)


type Box a
    = Box a


bad : Box String
bad =
    Box 1
