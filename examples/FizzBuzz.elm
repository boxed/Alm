module FizzBuzz exposing (main)


fizzbuzz : Int -> String
fizzbuzz n =
    case ( modBy 3 n, modBy 5 n ) of
        ( 0, 0 ) ->
            "FizzBuzz"

        ( 0, _ ) ->
            "Fizz"

        ( _, 0 ) ->
            "Buzz"

        _ ->
            String.fromInt n


main : String
main =
    List.range 1 15
        |> List.map fizzbuzz
        |> String.join " "
