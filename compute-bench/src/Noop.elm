module Noop exposing (..)

-- Startup-floor probe: a value program that does no work, so its wall-time is
-- pure process startup (dyld + runtime GC init + worker-thread spawn). The
-- harness subtracts this from each native workload to compare COMPUTE only,
-- matching how the JS/wasm figures are timed (warm, in-process, no startup).


bench : Int -> Int
bench _ =
    0


main : Int
main =
    0
