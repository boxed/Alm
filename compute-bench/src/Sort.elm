module Sort exposing (..)

-- Generate a pseudo-random list (LCG), sort it, checksum the result.


gen : Int -> Int -> List Int -> List Int
gen n seed acc =
    if n <= 0 then
        acc

    else
        let
            -- Park-Miller minstd; state*16807 stays under 2^53 so JS's f64 Int
            -- and the i64 backends agree exactly.
            s2 =
                modBy 2147483647 (seed * 16807)
        in
        gen (n - 1) s2 (s2 :: acc)


bench : Int -> Int
bench n =
    gen n 1 []
        |> List.sort
        |> List.foldl (\x a -> modBy 1000000007 (a * 31 + x)) 0


main : Int
main =
    bench 100000
