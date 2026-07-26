effect module Http where { command = MyCmd, subscription = MySub } exposing
    ( get, post, request
    , Header, header
    , Body, emptyBody, stringBody, jsonBody, fileBody, bytesBody
    , multipartBody, Part, stringPart, filePart, bytesPart
    , Expect, expectString, expectJson, expectBytes, expectWhatever, Error(..)
    , track, Progress(..), fractionSent, fractionReceived
    , cancel
    , riskyRequest
    , expectStringResponse, expectBytesResponse, Response(..), Metadata
    , task, Resolver, stringResolver, bytesResolver, riskyTask
    )

{-| A real `effect module` port of elm/http. `Header`/`Body`/`Part`/`Expect`/
`Resolver` are opaque (kernel-built, as in stock elm) — the builders delegate to
`Elm.Kernel.Http.*` (alm's existing request executor/reps, unchanged). Requests,
progress `track`, and `cancel` flow through the `_Platform` manager protocol; the
manager spawns each request as a cancellable process via `Elm.Kernel.Http.toTask`
and delivers progress through `onSelfMsg`. -}

import Basics exposing (..)
import Bytes exposing (Bytes)
import Bytes.Decode as Bytes
import Dict exposing (Dict)
import Elm.Kernel.Http
import File exposing (File)
import Json.Decode as Decode
import Json.Encode as Encode
import List
import Maybe exposing (Maybe(..))
import Platform
import Platform.Cmd exposing (Cmd)
import Platform.Sub exposing (Sub)
import Process
import Result exposing (Result(..))
import String
import Task exposing (Task)



-- REQUESTS


get : { url : String, expect : Expect msg } -> Cmd msg
get r =
    request
        { method = "GET"
        , headers = []
        , url = r.url
        , body = emptyBody
        , expect = r.expect
        , timeout = Nothing
        , tracker = Nothing
        }


post : { url : String, body : Body, expect : Expect msg } -> Cmd msg
post r =
    request
        { method = "POST"
        , headers = []
        , url = r.url
        , body = r.body
        , expect = r.expect
        , timeout = Nothing
        , tracker = Nothing
        }


request :
    { method : String
    , headers : List Header
    , url : String
    , body : Body
    , expect : Expect msg
    , timeout : Maybe Float
    , tracker : Maybe String
    }
    -> Cmd msg
request r =
    command (Request r)


riskyRequest :
    { method : String
    , headers : List Header
    , url : String
    , body : Body
    , expect : Expect msg
    , timeout : Maybe Float
    , tracker : Maybe String
    }
    -> Cmd msg
riskyRequest r =
    command (Request r)



-- HEADERS


type Header
    = Header


header : String -> String -> Header
header name value =
    Elm.Kernel.Http.header name value



-- BODY


type Body
    = Body


emptyBody : Body
emptyBody =
    Elm.Kernel.Http.emptyBody


jsonBody : Encode.Value -> Body
jsonBody value =
    Elm.Kernel.Http.jsonBody value


stringBody : String -> String -> Body
stringBody mime content =
    Elm.Kernel.Http.stringBody mime content


bytesBody : String -> Bytes -> Body
bytesBody mime content =
    Elm.Kernel.Http.bytesBody mime content


fileBody : File -> Body
fileBody file =
    Elm.Kernel.Http.fileBody file



-- PARTS


multipartBody : List Part -> Body
multipartBody parts =
    Elm.Kernel.Http.multipartBody parts


type Part
    = Part


stringPart : String -> String -> Part
stringPart name value =
    Elm.Kernel.Http.stringPart name value


filePart : String -> File -> Part
filePart name file =
    Elm.Kernel.Http.filePart name file


bytesPart : String -> String -> Bytes -> Part
bytesPart name mime content =
    Elm.Kernel.Http.bytesPart name mime content



-- EXPECT


type Expect msg
    = Expect


expectString : (Result Error String -> msg) -> Expect msg
expectString toMsg =
    Elm.Kernel.Http.expectString toMsg


expectJson : (Result Error a -> msg) -> Decode.Decoder a -> Expect msg
expectJson toMsg decoder =
    Elm.Kernel.Http.expectJson toMsg decoder


expectBytes : (Result Error a -> msg) -> Bytes.Decoder a -> Expect msg
expectBytes toMsg decoder =
    Elm.Kernel.Http.expectBytes toMsg decoder


expectWhatever : (Result Error () -> msg) -> Expect msg
expectWhatever toMsg =
    Elm.Kernel.Http.expectWhatever toMsg


type Error
    = BadUrl String
    | Timeout
    | NetworkError
    | BadStatus Int
    | BadBody String



-- EXPECT STRING/BYTES RESPONSE


expectStringResponse : (Result x a -> msg) -> (Response String -> Result x a) -> Expect msg
expectStringResponse toMsg toResult =
    Elm.Kernel.Http.expectStringResponse toMsg toResult


expectBytesResponse : (Result x a -> msg) -> (Response Bytes -> Result x a) -> Expect msg
expectBytesResponse toMsg toResult =
    Elm.Kernel.Http.expectBytesResponse toMsg toResult


type Response body
    = BadUrl_ String
    | Timeout_
    | NetworkError_
    | BadStatus_ Metadata body
    | GoodStatus_ Metadata body


type alias Metadata =
    { url : String
    , statusCode : Int
    , statusText : String
    , headers : Dict String String
    }



-- CANCEL


cancel : String -> Cmd msg
cancel tracker =
    command (Cancel tracker)



-- PROGRESS


track : String -> (Progress -> msg) -> Sub msg
track tracker toMsg =
    subscription (MySub tracker toMsg)


type Progress
    = Sending { sent : Int, size : Int }
    | Receiving { received : Int, size : Maybe Int }


fractionSent : { sent : Int, size : Int } -> Float
fractionSent p =
    if p.size == 0 then
        1

    else
        clamp 0 1 (toFloat p.sent / toFloat p.size)


fractionReceived : { received : Int, size : Maybe Int } -> Float
fractionReceived p =
    case p.size of
        Nothing ->
            0

        Just n ->
            if n == 0 then
                1

            else
                clamp 0 1 (toFloat p.received / toFloat n)



-- TASK


task :
    { method : String
    , headers : List Header
    , url : String
    , body : Body
    , resolver : Resolver x a
    , timeout : Maybe Float
    }
    -> Task x a
task r =
    Elm.Kernel.Http.task r


riskyTask :
    { method : String
    , headers : List Header
    , url : String
    , body : Body
    , resolver : Resolver x a
    , timeout : Maybe Float
    }
    -> Task x a
riskyTask r =
    Elm.Kernel.Http.task r


type Resolver x a
    = Resolver


stringResolver : (Response String -> Result x a) -> Resolver x a
stringResolver toResult =
    Elm.Kernel.Http.stringResolver toResult


bytesResolver : (Response Bytes -> Result x a) -> Resolver x a
bytesResolver toResult =
    Elm.Kernel.Http.bytesResolver toResult



-- COMMANDS and SUBSCRIPTIONS


type MyCmd msg
    = Cancel String
    | Request
        { method : String
        , headers : List Header
        , url : String
        , body : Body
        , expect : Expect msg
        , timeout : Maybe Float
        , tracker : Maybe String
        }


cmdMap : (a -> b) -> MyCmd a -> MyCmd b
cmdMap func cmd =
    case cmd of
        Cancel tracker ->
            Cancel tracker

        Request r ->
            Request
                { method = r.method
                , headers = r.headers
                , url = r.url
                , body = r.body
                , expect = Elm.Kernel.Http.mapExpect func r.expect
                , timeout = r.timeout
                , tracker = r.tracker
                }


type MySub msg
    = MySub String (Progress -> msg)


subMap : (a -> b) -> MySub a -> MySub b
subMap func (MySub tracker toMsg) =
    MySub tracker (toMsg >> func)



-- EFFECT MANAGER


type alias State msg =
    { reqs : Dict String Process.Id
    , subs : List (MySub msg)
    }


init : Task Never (State msg)
init =
    Task.succeed (State Dict.empty [])


type alias MyRouter msg =
    Platform.Router msg SelfMsg


onEffects : MyRouter msg -> List (MyCmd msg) -> List (MySub msg) -> State msg -> Task Never (State msg)
onEffects router cmds subs state =
    Task.andThen
        (\reqs -> Task.succeed (State reqs subs))
        (updateReqs router cmds state.reqs)


updateReqs : MyRouter msg -> List (MyCmd msg) -> Dict String Process.Id -> Task x (Dict String Process.Id)
updateReqs router cmds reqs =
    case cmds of
        [] ->
            Task.succeed reqs

        cmd :: otherCmds ->
            case cmd of
                Cancel tracker ->
                    case Dict.get tracker reqs of
                        Nothing ->
                            updateReqs router otherCmds reqs

                        Just pid ->
                            Task.andThen
                                (\_ -> updateReqs router otherCmds (Dict.remove tracker reqs))
                                (Process.kill pid)

                Request req ->
                    Task.andThen
                        (\pid ->
                            case req.tracker of
                                Nothing ->
                                    updateReqs router otherCmds reqs

                                Just tracker ->
                                    updateReqs router otherCmds (Dict.insert tracker pid reqs)
                        )
                        (Process.spawn (Elm.Kernel.Http.toTask router (Platform.sendToApp router) req))


type alias SelfMsg =
    ( String, Progress )


onSelfMsg : MyRouter msg -> SelfMsg -> State msg -> Task Never (State msg)
onSelfMsg router ( tracker, progress ) state =
    Task.andThen
        (\_ -> Task.succeed state)
        (Task.sequence (List.filterMap (maybeSend router tracker progress) state.subs))


maybeSend : MyRouter msg -> String -> Progress -> MySub msg -> Maybe (Task x ())
maybeSend router desiredTracker progress (MySub actualTracker toMsg) =
    if desiredTracker == actualTracker then
        Just (Platform.sendToApp router (toMsg progress))

    else
        Nothing
