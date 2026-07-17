module BinaryTrees exposing (..)

-- Allocation / GC stress: build many short-lived trees and walk them.
-- Custom type nodes are heap-allocated and immediately discarded.


type Tree
    = Leaf
    | Node Tree Tree


build : Int -> Tree
build d =
    if d <= 0 then
        Leaf

    else
        Node (build (d - 1)) (build (d - 1))


check : Tree -> Int
check t =
    case t of
        Leaf ->
            1

        Node l r ->
            1 + check l + check r


run : Int -> Int -> Int
run k acc =
    if k <= 0 then
        acc

    else
        run (k - 1) (acc + check (build 14))


bench : Int -> Int
bench n =
    run n 0


main : Int
main =
    bench 60
