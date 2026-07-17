module Fib exposing (..)

-- Naive recursion: call overhead + integer arithmetic, ~no allocation.


fib : Int -> Int
fib n =
    if n <= 1 then
        n

    else
        fib (n - 1) + fib (n - 2)


bench : Int -> Int
bench n =
    fib n


main : Int
main =
    bench 35
