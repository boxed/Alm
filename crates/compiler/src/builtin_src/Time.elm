effect module Time where { subscription = MySub } exposing
    ( Posix, now, every, posixToMillis, millisToPosix
    , Zone, utc, here
    , toYear, toMonth, toDay, toWeekday, toHour, toMinute, toSecond, toMillis
    , Month(..), Weekday(..)
    , customZone
    , ZoneName(..), getZoneName
    )

{-| A real `effect module` port of elm/time. The calendar math is pure Elm
(unchanged from elm/time); only `now`, `here`, `getZoneName`, and the manager's
`setInterval` are kernel primitives. `Time.every` is a genuine subscription
handled by this manager through the `_Platform` protocol. -}

import Basics exposing (..)
import Dict
import Elm.Kernel.Time
import List exposing ((::))
import Maybe exposing (Maybe(..))
import Platform
import Platform.Sub exposing (Sub)
import Process
import String exposing (String)
import Task exposing (Task)



-- POSIX


type Posix
    = Posix Int


now : Task x Posix
now =
    Elm.Kernel.Time.now millisToPosix


posixToMillis : Posix -> Int
posixToMillis (Posix millis) =
    millis


millisToPosix : Int -> Posix
millisToPosix =
    Posix



-- TIME ZONES


type Zone
    = Zone Int (List Era)


type alias Era =
    { start : Int, offset : Int }


utc : Zone
utc =
    Zone 0 []


here : Task x Zone
here =
    Elm.Kernel.Time.here ()



-- DATES


toYear : Zone -> Posix -> Int
toYear zone time =
    (toCivil (toAdjustedMinutes zone time)).year


toMonth : Zone -> Posix -> Month
toMonth zone time =
    case (toCivil (toAdjustedMinutes zone time)).month of
        1 -> Jan
        2 -> Feb
        3 -> Mar
        4 -> Apr
        5 -> May
        6 -> Jun
        7 -> Jul
        8 -> Aug
        9 -> Sep
        10 -> Oct
        11 -> Nov
        _ -> Dec


toDay : Zone -> Posix -> Int
toDay zone time =
    (toCivil (toAdjustedMinutes zone time)).day


toWeekday : Zone -> Posix -> Weekday
toWeekday zone time =
    case modBy 7 (flooredDiv (toAdjustedMinutes zone time) (60 * 24)) of
        0 -> Thu
        1 -> Fri
        2 -> Sat
        3 -> Sun
        4 -> Mon
        5 -> Tue
        _ -> Wed


toHour : Zone -> Posix -> Int
toHour zone time =
    modBy 24 (flooredDiv (toAdjustedMinutes zone time) 60)


toMinute : Zone -> Posix -> Int
toMinute zone time =
    modBy 60 (toAdjustedMinutes zone time)


toSecond : Zone -> Posix -> Int
toSecond _ time =
    modBy 60 (flooredDiv (posixToMillis time) 1000)


toMillis : Zone -> Posix -> Int
toMillis _ time =
    modBy 1000 (posixToMillis time)



-- DATE HELPERS


toAdjustedMinutes : Zone -> Posix -> Int
toAdjustedMinutes (Zone defaultOffset eras) time =
    toAdjustedMinutesHelp defaultOffset (flooredDiv (posixToMillis time) 60000) eras


toAdjustedMinutesHelp : Int -> Int -> List Era -> Int
toAdjustedMinutesHelp defaultOffset posixMinutes eras =
    case eras of
        [] ->
            posixMinutes + defaultOffset

        era :: olderEras ->
            if era.start < posixMinutes then
                posixMinutes + era.offset

            else
                toAdjustedMinutesHelp defaultOffset posixMinutes olderEras


toCivil : Int -> { year : Int, month : Int, day : Int }
toCivil minutes =
    let
        rawDay =
            flooredDiv minutes (60 * 24) + 719468

        era =
            (if rawDay >= 0 then rawDay else rawDay - 146096) // 146097

        dayOfEra =
            rawDay - era * 146097

        yearOfEra =
            (dayOfEra - dayOfEra // 1460 + dayOfEra // 36524 - dayOfEra // 146096) // 365

        year =
            yearOfEra + era * 400

        dayOfYear =
            dayOfEra - (365 * yearOfEra + yearOfEra // 4 - yearOfEra // 100)

        mp =
            (5 * dayOfYear + 2) // 153

        month =
            mp + (if mp < 10 then 3 else -9)
    in
    { year = year + (if month <= 2 then 1 else 0)
    , month = month
    , day = dayOfYear - (153 * mp + 2) // 5 + 1
    }


flooredDiv : Int -> Float -> Int
flooredDiv numerator denominator =
    floor (toFloat numerator / denominator)



-- WEEKDAYS AND MONTHS


type Weekday
    = Mon
    | Tue
    | Wed
    | Thu
    | Fri
    | Sat
    | Sun


type Month
    = Jan
    | Feb
    | Mar
    | Apr
    | May
    | Jun
    | Jul
    | Aug
    | Sep
    | Oct
    | Nov
    | Dec



-- SUBSCRIPTIONS


every : Float -> (Posix -> msg) -> Sub msg
every interval tagger =
    subscription (Every interval tagger)


type MySub msg
    = Every Float (Posix -> msg)


subMap : (a -> b) -> MySub a -> MySub b
subMap f (Every interval tagger) =
    Every interval (f << tagger)



-- EFFECT MANAGER


type alias State msg =
    { taggers : Taggers msg
    , processes : Processes
    }


type alias Processes =
    Dict.Dict Float Process.Id


type alias Taggers msg =
    Dict.Dict Float (List (Posix -> msg))


init : Task Never (State msg)
init =
    Task.succeed (State Dict.empty Dict.empty)


onEffects : Platform.Router msg Float -> List (MySub msg) -> State msg -> Task Never (State msg)
onEffects router subs { processes } =
    let
        newTaggers =
            List.foldl addMySub Dict.empty subs

        leftStep interval taggers ( spawns, existing, kills ) =
            ( interval :: spawns, existing, kills )

        bothStep interval taggers id ( spawns, existing, kills ) =
            ( spawns, Dict.insert interval id existing, kills )

        rightStep _ id ( spawns, existing, kills ) =
            ( spawns, existing, Task.andThen (\_ -> kills) (Process.kill id) )

        ( spawnList, existingDict, killTask ) =
            Dict.merge
                leftStep
                bothStep
                rightStep
                newTaggers
                processes
                ( [], Dict.empty, Task.succeed () )
    in
    killTask
        |> Task.andThen (\_ -> spawnHelp router spawnList existingDict)
        |> Task.andThen (\newProcesses -> Task.succeed (State newTaggers newProcesses))


addMySub : MySub msg -> Taggers msg -> Taggers msg
addMySub (Every interval tagger) state =
    case Dict.get interval state of
        Nothing ->
            Dict.insert interval [ tagger ] state

        Just taggers ->
            Dict.insert interval (tagger :: taggers) state


spawnHelp : Platform.Router msg Float -> List Float -> Processes -> Task.Task x Processes
spawnHelp router intervals processes =
    case intervals of
        [] ->
            Task.succeed processes

        interval :: rest ->
            let
                spawnTimer =
                    Process.spawn (setInterval interval (Platform.sendToSelf router interval))

                spawnRest id =
                    spawnHelp router rest (Dict.insert interval id processes)
            in
            spawnTimer
                |> Task.andThen spawnRest


onSelfMsg : Platform.Router msg Float -> Float -> State msg -> Task Never (State msg)
onSelfMsg router interval state =
    case Dict.get interval state.taggers of
        Nothing ->
            Task.succeed state

        Just taggers ->
            let
                tellTaggers time =
                    Task.sequence (List.map (\tagger -> Platform.sendToApp router (tagger time)) taggers)
            in
            now
                |> Task.andThen tellTaggers
                |> Task.andThen (\_ -> Task.succeed state)


setInterval : Float -> Task Never () -> Task x Never
setInterval =
    Elm.Kernel.Time.setInterval



-- FOR PACKAGE AUTHORS


customZone : Int -> List { start : Int, offset : Int } -> Zone
customZone =
    Zone


getZoneName : Task x ZoneName
getZoneName =
    Elm.Kernel.Time.getZoneName ()


type ZoneName
    = Name String
    | Offset Int
