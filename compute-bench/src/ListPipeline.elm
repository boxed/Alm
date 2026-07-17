module ListPipeline exposing (..)

-- Immutable list allocation: range -> map -> filter -> fold.


bench : Int -> Int
bench n =
    List.range 1 n
        |> List.map (\x -> x * 2)
        |> List.filter (\x -> modBy 3 x == 0)
        |> List.foldl (\x a -> modBy 1000000007 (a + x)) 0


main : Int
main =
    bench 1000000
