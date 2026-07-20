module ListFilter exposing (..)

-- Allocation: build a range, filter it (allocates ~1/3 as a new list), sum.


bench : Int -> Int
bench n =
    List.range 1 n
        |> List.filter (\x -> modBy 3 x == 0)
        |> List.sum


main : Int
main =
    bench 1000000
