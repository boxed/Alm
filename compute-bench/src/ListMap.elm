module ListMap exposing (..)

-- Allocation: build a range, map over it (allocates a new list), then sum.


bench : Int -> Int
bench n =
    List.range 1 n
        |> List.map (\x -> x * 2)
        |> List.sum


main : Int
main =
    bench 1000000
