module Mandelbrot exposing (..)

-- Float math, no allocation: count points inside the Mandelbrot set on an
-- N x N grid, `maxIter` iterations each.


maxIter : Int
maxIter =
    100


iter : Float -> Float -> Float -> Float -> Int -> Int
iter zr zi cr ci n =
    if n >= maxIter then
        1

    else if zr * zr + zi * zi > 4.0 then
        0

    else
        iter (zr * zr - zi * zi + cr) (2.0 * zr * zi + ci) cr ci (n + 1)


row : Int -> Int -> Int -> Int -> Int
row size y x acc =
    if x >= size then
        acc

    else
        let
            cr =
                (toFloat x / toFloat size) * 3.0 - 2.0

            ci =
                (toFloat y / toFloat size) * 3.0 - 1.5
        in
        row size y (x + 1) (acc + iter 0.0 0.0 cr ci 0)


grid : Int -> Int -> Int -> Int
grid size y acc =
    if y >= size then
        acc

    else
        grid size (y + 1) (row size y 0 acc)


bench : Int -> Int
bench size =
    grid size 0 0


main : Int
main =
    bench 400
