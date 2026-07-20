module ListFold exposing (..)

-- Pure traversal: build a range, fold it to a scalar (no extra allocation).


bench : Int -> Int
bench n =
    List.range 1 n
        |> List.foldl (\x a -> modBy 1000000007 (a + x)) 0


main : Int
main =
    bench 1000000
