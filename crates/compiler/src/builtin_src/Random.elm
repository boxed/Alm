effect module Random where { command = MyCmd } exposing
    ( Generator, Seed
    , int, float, maxInt, minInt
    , constant, pair, list, uniform, weighted
    , map, map2, map3, map4, map5, andThen, lazy
    , step, initialSeed, independentSeed
    , generate
    )

{-| A real `effect module` port of elm/random. `Generator`/`Seed` are opaque
types whose runtime representation is owned by the backend intrinsics
(`Elm.Kernel.Random.*`) — the source never inspects them, so each backend keeps
its optimized representation (JS closures, wasm-gc reified data). Only
`generate` (a command) and the manager are real Elm; the manager seeds from
`Time.now`. -}

import Basics exposing (..)
import Elm.Kernel.Random
import List exposing ((::))
import Platform
import Platform.Cmd exposing (Cmd)
import Task exposing (Task)
import Time
import Tuple



-- GENERATORS (opaque; ops delegate to the backend intrinsics)


type Generator a
    = Generator


type Seed
    = Seed


int : Int -> Int -> Generator Int
int lo hi =
    Elm.Kernel.Random.int lo hi


float : Float -> Float -> Generator Float
float lo hi =
    Elm.Kernel.Random.float lo hi


maxInt : Int
maxInt =
    2147483647


minInt : Int
minInt =
    -2147483648


constant : a -> Generator a
constant value =
    Elm.Kernel.Random.constant value


pair : Generator a -> Generator b -> Generator ( a, b )
pair genA genB =
    Elm.Kernel.Random.pair genA genB


list : Int -> Generator a -> Generator (List a)
list n gen =
    Elm.Kernel.Random.list n gen


uniform : a -> List a -> Generator a
uniform value valueList =
    Elm.Kernel.Random.uniform value valueList


weighted : ( Float, a ) -> List ( Float, a ) -> Generator a
weighted first others =
    Elm.Kernel.Random.weighted first others


map : (a -> b) -> Generator a -> Generator b
map func gen =
    Elm.Kernel.Random.map func gen


map2 : (a -> b -> c) -> Generator a -> Generator b -> Generator c
map2 func genA genB =
    Elm.Kernel.Random.map2 func genA genB


map3 : (a -> b -> c -> d) -> Generator a -> Generator b -> Generator c -> Generator d
map3 func genA genB genC =
    Elm.Kernel.Random.map3 func genA genB genC


map4 : (a -> b -> c -> d -> e) -> Generator a -> Generator b -> Generator c -> Generator d -> Generator e
map4 func genA genB genC genD =
    Elm.Kernel.Random.map4 func genA genB genC genD


map5 : (a -> b -> c -> d -> e -> f) -> Generator a -> Generator b -> Generator c -> Generator d -> Generator e -> Generator f
map5 func genA genB genC genD genE =
    Elm.Kernel.Random.map5 func genA genB genC genD genE


andThen : (a -> Generator b) -> Generator a -> Generator b
andThen callback gen =
    Elm.Kernel.Random.andThen callback gen


lazy : (() -> Generator a) -> Generator a
lazy callback =
    Elm.Kernel.Random.lazy callback


step : Generator a -> Seed -> ( a, Seed )
step gen seed =
    Elm.Kernel.Random.step gen seed


initialSeed : Int -> Seed
initialSeed x =
    Elm.Kernel.Random.initialSeed x


independentSeed : Generator Seed
independentSeed =
    Elm.Kernel.Random.independentSeed



-- EFFECT MANAGER


generate : (a -> msg) -> Generator a -> Cmd msg
generate tagger generator =
    command (Generate (map tagger generator))


type MyCmd msg
    = Generate (Generator msg)


cmdMap : (a -> b) -> MyCmd a -> MyCmd b
cmdMap func (Generate generator) =
    Generate (map func generator)


init : Task Never Seed
init =
    Task.andThen (\time -> Task.succeed (initialSeed (Time.posixToMillis time))) Time.now


onEffects : Platform.Router msg Never -> List (MyCmd msg) -> Seed -> Task Never Seed
onEffects router commands seed =
    case commands of
        [] ->
            Task.succeed seed

        (Generate generator) :: rest ->
            let
                ( value, newSeed ) =
                    step generator seed
            in
            Task.andThen
                (\_ -> onEffects router rest newSeed)
                (Platform.sendToApp router value)


onSelfMsg : Platform.Router msg Never -> Never -> Seed -> Task Never Seed
onSelfMsg _ _ seed =
    Task.succeed seed
