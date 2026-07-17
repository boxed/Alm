module FloatSum exposing (..)

-- Float-list workload: build a large `List Float`, map an affine transform over
-- it, then fold a sum-of-squares. Exercises List Float construction +
-- map + foldl — where wasm-gc currently boxes every element (one T_FLOAT heap
-- allocation per float) versus native's contiguous unboxed f64 backing.


bench : Int -> Int
bench n =
    let
        xs =
            List.map (\i -> toFloat i) (List.range 1 n)

        scaled =
            List.map (\x -> x * 1.5 + 0.5) xs
    in
    round (List.foldl (\x acc -> acc + x * x) 0.0 scaled)


main : Int
main =
    bench 1000000
