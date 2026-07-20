port module ElmBench exposing (main)

-- Headless harness so the OFFICIAL elm compiler's output can be benchmarked on
-- the same workloads. Official elm exposes only `Elm.X.init` (not arbitrary
-- top-level values the way alm does), so we drive each workload's `bench`
-- through a Platform.worker: JS sends {name,size} on an incoming port, we
-- compute and send the result back out. elm 0.19 processes port sends
-- synchronously, so the JS harness can time each round-trip warm.

import BinaryTrees
import DictOps
import Fib
import FloatSum
import ListFilter
import ListFold
import ListMap
import Mandelbrot
import Platform
import Sort


port toBench : ({ name : String, size : Int } -> msg) -> Sub msg


port fromBench : Int -> Cmd msg


run : String -> Int -> Int
run name size =
    case name of
        "Fib" ->
            Fib.bench size

        "BinaryTrees" ->
            BinaryTrees.bench size

        "ListMap" ->
            ListMap.bench size

        "ListFilter" ->
            ListFilter.bench size

        "ListFold" ->
            ListFold.bench size

        "DictOps" ->
            DictOps.bench size

        "Mandelbrot" ->
            Mandelbrot.bench size

        "Sort" ->
            Sort.bench size

        "FloatSum" ->
            FloatSum.bench size

        _ ->
            0


main : Program () () { name : String, size : Int }
main =
    Platform.worker
        { init = \_ -> ( (), Cmd.none )
        , update = \msg model -> ( model, fromBench (run msg.name msg.size) )
        , subscriptions = \_ -> toBench identity
        }
