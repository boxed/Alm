//! The built-in standard library surface: the parts of `elm/core` that alm
//! knows about natively. The real compiler compiles elm/core from source;
//! alm ships type signatures here and JavaScript implementations in the
//! runtime prelude (`generate/runtime.js`).
//!
//! Signatures are written as Elm type syntax and parsed with alm's own
//! type parser.

use crate::ast::canonical::Type;
use crate::ast::source::{self, Associativity};
use crate::data::Name;
use crate::parse::Parser;

pub struct BuiltinValue {
    pub module: &'static str,
    pub name: &'static str,
    pub signature: &'static str,
}

/// (module, name, signature)
const V: fn(&'static str, &'static str, &'static str) -> BuiltinValue =
    |module, name, signature| BuiltinValue {
        module,
        name,
        signature,
    };

pub fn values() -> &'static [BuiltinValue] {
    static VALUES: std::sync::OnceLock<Vec<BuiltinValue>> = std::sync::OnceLock::new();
    VALUES.get_or_init(|| {
        let mut table = vec![
            // Basics — arithmetic
            V("Basics", "add", "number -> number -> number"),
            V("Basics", "sub", "number -> number -> number"),
            V("Basics", "mul", "number -> number -> number"),
            V("Basics", "fdiv", "Float -> Float -> Float"),
            V("Basics", "idiv", "Int -> Int -> Int"),
            V("Basics", "pow", "number -> number -> number"),
            V("Basics", "negate", "number -> number"),
            V("Basics", "abs", "number -> number"),
            V("Basics", "clamp", "number -> number -> number -> number"),
            V("Basics", "sqrt", "Float -> Float"),
            V("Basics", "modBy", "Int -> Int -> Int"),
            V("Basics", "remainderBy", "Int -> Int -> Int"),
            V("Basics", "logBase", "Float -> Float -> Float"),
            V("Basics", "e", "Float"),
            V("Basics", "pi", "Float"),
            V("Basics", "cos", "Float -> Float"),
            V("Basics", "sin", "Float -> Float"),
            V("Basics", "tan", "Float -> Float"),
            V("Basics", "acos", "Float -> Float"),
            V("Basics", "asin", "Float -> Float"),
            V("Basics", "atan", "Float -> Float"),
            V("Basics", "atan2", "Float -> Float -> Float"),
            // Basics — Int/Float conversion
            V("Basics", "toFloat", "Int -> Float"),
            V("Basics", "round", "Float -> Int"),
            V("Basics", "floor", "Float -> Int"),
            V("Basics", "ceiling", "Float -> Int"),
            V("Basics", "truncate", "Float -> Int"),
            // Basics — comparison and equality
            V("Basics", "eq", "a -> a -> Bool"),
            V("Basics", "neq", "a -> a -> Bool"),
            V("Basics", "lt", "comparable -> comparable -> Bool"),
            V("Basics", "gt", "comparable -> comparable -> Bool"),
            V("Basics", "le", "comparable -> comparable -> Bool"),
            V("Basics", "ge", "comparable -> comparable -> Bool"),
            V("Basics", "min", "comparable -> comparable -> comparable"),
            V("Basics", "max", "comparable -> comparable -> comparable"),
            V("Basics", "compare", "comparable -> comparable -> Order"),
            // Basics — booleans
            V("Basics", "not", "Bool -> Bool"),
            V("Basics", "and", "Bool -> Bool -> Bool"),
            V("Basics", "or", "Bool -> Bool -> Bool"),
            V("Basics", "xor", "Bool -> Bool -> Bool"),
            // Basics — appendable
            V("Basics", "append", "appendable -> appendable -> appendable"),
            // Basics — function helpers
            V("Basics", "identity", "a -> a"),
            V("Basics", "never", "Never -> a"),
            V("Basics", "always", "a -> b -> a"),
            V("Basics", "apL", "(a -> b) -> a -> b"),
            V("Basics", "apR", "a -> (a -> b) -> b"),
            V("Basics", "composeL", "(b -> c) -> (a -> b) -> a -> c"),
            V("Basics", "composeR", "(a -> b) -> (b -> c) -> a -> c"),
            // List
            V("List", "cons", "a -> List a -> List a"),
            V("List", "singleton", "a -> List a"),
            V("List", "repeat", "Int -> a -> List a"),
            V("List", "range", "Int -> Int -> List Int"),
            V("List", "map", "(a -> b) -> List a -> List b"),
            V("List", "indexedMap", "(Int -> a -> b) -> List a -> List b"),
            V("List", "foldl", "(a -> b -> b) -> b -> List a -> b"),
            V("List", "foldr", "(a -> b -> b) -> b -> List a -> b"),
            V("List", "filter", "(a -> Bool) -> List a -> List a"),
            V("List", "filterMap", "(a -> Maybe b) -> List a -> List b"),
            V("List", "length", "List a -> Int"),
            V("List", "reverse", "List a -> List a"),
            V("List", "member", "a -> List a -> Bool"),
            V("List", "all", "(a -> Bool) -> List a -> Bool"),
            V("List", "any", "(a -> Bool) -> List a -> Bool"),
            V("List", "maximum", "List comparable -> Maybe comparable"),
            V("List", "minimum", "List comparable -> Maybe comparable"),
            V("List", "sum", "List number -> number"),
            V("List", "product", "List number -> number"),
            V("List", "append", "List a -> List a -> List a"),
            V("List", "concat", "List (List a) -> List a"),
            V("List", "concatMap", "(a -> List b) -> List a -> List b"),
            V("List", "intersperse", "a -> List a -> List a"),
            V("List", "map2", "(a -> b -> result) -> List a -> List b -> List result"),
            V("List", "sort", "List comparable -> List comparable"),
            V("List", "sortBy", "(a -> comparable) -> List a -> List a"),
            V("List", "isEmpty", "List a -> Bool"),
            V("List", "head", "List a -> Maybe a"),
            V("List", "tail", "List a -> Maybe (List a)"),
            V("List", "take", "Int -> List a -> List a"),
            V("List", "drop", "Int -> List a -> List a"),
            V("List", "partition", "(a -> Bool) -> List a -> ( List a, List a )"),
            V("List", "unzip", "List ( a, b ) -> ( List a, List b )"),
            // String
            V("String", "isEmpty", "String -> Bool"),
            V("String", "length", "String -> Int"),
            V("String", "reverse", "String -> String"),
            V("String", "repeat", "Int -> String -> String"),
            V("String", "replace", "String -> String -> String -> String"),
            V("String", "append", "String -> String -> String"),
            V("String", "concat", "List String -> String"),
            V("String", "split", "String -> String -> List String"),
            V("String", "join", "String -> List String -> String"),
            V("String", "words", "String -> List String"),
            V("String", "lines", "String -> List String"),
            V("String", "slice", "Int -> Int -> String -> String"),
            V("String", "left", "Int -> String -> String"),
            V("String", "right", "Int -> String -> String"),
            V("String", "dropLeft", "Int -> String -> String"),
            V("String", "dropRight", "Int -> String -> String"),
            V("String", "contains", "String -> String -> Bool"),
            V("String", "startsWith", "String -> String -> Bool"),
            V("String", "endsWith", "String -> String -> Bool"),
            V("String", "toInt", "String -> Maybe Int"),
            V("String", "fromInt", "Int -> String"),
            V("String", "toFloat", "String -> Maybe Float"),
            V("String", "fromFloat", "Float -> String"),
            V("String", "fromChar", "Char -> String"),
            V("String", "toList", "String -> List Char"),
            V("String", "fromList", "List Char -> String"),
            V("String", "toUpper", "String -> String"),
            V("String", "toLower", "String -> String"),
            V("String", "trim", "String -> String"),
            V("String", "trimLeft", "String -> String"),
            V("String", "trimRight", "String -> String"),
            V("String", "pad", "Int -> Char -> String -> String"),
            V("String", "padLeft", "Int -> Char -> String -> String"),
            V("String", "padRight", "Int -> Char -> String -> String"),
            V("String", "filter", "(Char -> Bool) -> String -> String"),
            V("String", "map", "(Char -> Char) -> String -> String"),
            // Char
            V("Char", "toCode", "Char -> Int"),
            V("Char", "fromCode", "Int -> Char"),
            V("Char", "isDigit", "Char -> Bool"),
            V("Char", "isAlpha", "Char -> Bool"),
            V("Char", "isUpper", "Char -> Bool"),
            V("Char", "isLower", "Char -> Bool"),
            V("Char", "toUpper", "Char -> Char"),
            V("Char", "toLower", "Char -> Char"),
            // Maybe
            V("Maybe", "withDefault", "a -> Maybe a -> a"),
            V("Maybe", "map", "(a -> b) -> Maybe a -> Maybe b"),
            V("Maybe", "map2", "(a -> b -> value) -> Maybe a -> Maybe b -> Maybe value"),
            V("Maybe", "andThen", "(a -> Maybe b) -> Maybe a -> Maybe b"),
            // Result
            V("Result", "withDefault", "a -> Result x a -> a"),
            V("Result", "map", "(a -> value) -> Result x a -> Result x value"),
            V("Result", "mapError", "(x -> y) -> Result x a -> Result y a"),
            V("Result", "andThen", "(a -> Result x b) -> Result x a -> Result x b"),
            V("Result", "toMaybe", "Result x a -> Maybe a"),
            V("Result", "fromMaybe", "x -> Maybe a -> Result x a"),
            // Tuple
            V("Tuple", "pair", "a -> b -> ( a, b )"),
            V("Tuple", "first", "( a, b ) -> a"),
            V("Tuple", "second", "( a, b ) -> b"),
            V("Tuple", "mapFirst", "(a -> x) -> ( a, b ) -> ( x, b )"),
            V("Tuple", "mapSecond", "(b -> y) -> ( a, b ) -> ( a, y )"),
            V("Tuple", "mapBoth", "(a -> x) -> (b -> y) -> ( a, b ) -> ( x, y )"),
            // Debug
            V("Debug", "toString", "a -> String"),
            V("Debug", "log", "String -> a -> a"),
            V("Debug", "todo", "String -> a"),
            // Basics — extras
            V("Basics", "isNaN", "Float -> Bool"),
            V("Basics", "isInfinite", "Float -> Bool"),
            V("Basics", "degrees", "Float -> Float"),
            V("Basics", "radians", "Float -> Float"),
            V("Basics", "turns", "Float -> Float"),
            V("Basics", "toPolar", "( Float, Float ) -> ( Float, Float )"),
            V("Basics", "fromPolar", "( Float, Float ) -> ( Float, Float )"),
            // List — extras
            V("List", "sortWith", "(a -> a -> Order) -> List a -> List a"),
            V("List", "map3", "(a -> b -> c -> result) -> List a -> List b -> List c -> List result"),
            // String — extras
            V("String", "uncons", "String -> Maybe ( Char, String )"),
            V("String", "cons", "Char -> String -> String"),
            V("String", "indexes", "String -> String -> List Int"),
            V("String", "indices", "String -> String -> List Int"),
            V("String", "any", "(Char -> Bool) -> String -> Bool"),
            V("String", "all", "(Char -> Bool) -> String -> Bool"),
            V("String", "foldl", "(Char -> b -> b) -> b -> String -> b"),
            V("String", "foldr", "(Char -> b -> b) -> b -> String -> b"),
            // Char — extras
            V("Char", "isAlphaNum", "Char -> Bool"),
            V("Char", "isHexDigit", "Char -> Bool"),
            V("Char", "isOctDigit", "Char -> Bool"),
            // Maybe — extras
            V("Maybe", "map3", "(a -> b -> c -> value) -> Maybe a -> Maybe b -> Maybe c -> Maybe value"),
            V("Maybe", "map4", "(a -> b -> c -> d -> value) -> Maybe a -> Maybe b -> Maybe c -> Maybe d -> Maybe value"),
            // Result — extras
            V("Result", "map2", "(a -> b -> value) -> Result x a -> Result x b -> Result x value"),
            V("Result", "map3", "(a -> b -> c -> value) -> Result x a -> Result x b -> Result x c -> Result x value"),
            V("Result", "map4", "(a -> b -> c -> d -> value) -> Result x a -> Result x b -> Result x c -> Result x d -> Result x value"),
            V("Result", "map5", "(a -> b -> c -> d -> e -> value) -> Result x a -> Result x b -> Result x c -> Result x d -> Result x e -> Result x value"),
            V("Maybe", "map5", "(a -> b -> c -> d -> e -> value) -> Maybe a -> Maybe b -> Maybe c -> Maybe d -> Maybe e -> Maybe value"),
            V("List", "map4", "(a -> b -> c -> d -> result) -> List a -> List b -> List c -> List d -> List result"),
            V("List", "map5", "(a -> b -> c -> d -> e -> result) -> List a -> List b -> List c -> List d -> List e -> List result"),
            // Dict
            V("Dict", "empty", "Dict k v"),
            V("Dict", "singleton", "comparable -> v -> Dict comparable v"),
            V("Dict", "insert", "comparable -> v -> Dict comparable v -> Dict comparable v"),
            V("Dict", "update", "comparable -> (Maybe v -> Maybe v) -> Dict comparable v -> Dict comparable v"),
            V("Dict", "remove", "comparable -> Dict comparable v -> Dict comparable v"),
            V("Dict", "isEmpty", "Dict k v -> Bool"),
            V("Dict", "member", "comparable -> Dict comparable v -> Bool"),
            V("Dict", "get", "comparable -> Dict comparable v -> Maybe v"),
            V("Dict", "size", "Dict k v -> Int"),
            V("Dict", "keys", "Dict k v -> List k"),
            V("Dict", "values", "Dict k v -> List v"),
            V("Dict", "toList", "Dict k v -> List ( k, v )"),
            V("Dict", "fromList", "List ( comparable, v ) -> Dict comparable v"),
            V("Dict", "map", "(k -> a -> b) -> Dict k a -> Dict k b"),
            V("Dict", "foldl", "(k -> v -> b -> b) -> b -> Dict k v -> b"),
            V("Dict", "foldr", "(k -> v -> b -> b) -> b -> Dict k v -> b"),
            V("Dict", "filter", "(comparable -> v -> Bool) -> Dict comparable v -> Dict comparable v"),
            V("Dict", "partition", "(comparable -> v -> Bool) -> Dict comparable v -> ( Dict comparable v, Dict comparable v )"),
            V("Dict", "union", "Dict comparable v -> Dict comparable v -> Dict comparable v"),
            V("Dict", "intersect", "Dict comparable v -> Dict comparable v -> Dict comparable v"),
            V("Dict", "diff", "Dict comparable a -> Dict comparable b -> Dict comparable a"),
            V("Dict", "merge", "(comparable -> a -> result -> result) -> (comparable -> a -> b -> result -> result) -> (comparable -> b -> result -> result) -> Dict comparable a -> Dict comparable b -> result -> result"),
            // Set
            V("Set", "empty", "Set a"),
            V("Set", "singleton", "comparable -> Set comparable"),
            V("Set", "insert", "comparable -> Set comparable -> Set comparable"),
            V("Set", "remove", "comparable -> Set comparable -> Set comparable"),
            V("Set", "isEmpty", "Set a -> Bool"),
            V("Set", "member", "comparable -> Set comparable -> Bool"),
            V("Set", "size", "Set a -> Int"),
            V("Set", "toList", "Set a -> List a"),
            V("Set", "fromList", "List comparable -> Set comparable"),
            V("Set", "map", "(comparable -> comparable2) -> Set comparable -> Set comparable2"),
            V("Set", "foldl", "(a -> b -> b) -> b -> Set a -> b"),
            V("Set", "foldr", "(a -> b -> b) -> b -> Set a -> b"),
            V("Set", "filter", "(comparable -> Bool) -> Set comparable -> Set comparable"),
            V("Set", "partition", "(comparable -> Bool) -> Set comparable -> ( Set comparable, Set comparable )"),
            V("Set", "union", "Set comparable -> Set comparable -> Set comparable"),
            V("Set", "intersect", "Set comparable -> Set comparable -> Set comparable"),
            V("Set", "diff", "Set comparable -> Set comparable -> Set comparable"),
            // Array
            V("Array", "empty", "Array a"),
            V("Array", "initialize", "Int -> (Int -> a) -> Array a"),
            V("Array", "repeat", "Int -> a -> Array a"),
            V("Array", "fromList", "List a -> Array a"),
            V("Array", "isEmpty", "Array a -> Bool"),
            V("Array", "length", "Array a -> Int"),
            V("Array", "get", "Int -> Array a -> Maybe a"),
            V("Array", "set", "Int -> a -> Array a -> Array a"),
            V("Array", "push", "a -> Array a -> Array a"),
            V("Array", "toList", "Array a -> List a"),
            V("Array", "toIndexedList", "Array a -> List ( Int, a )"),
            V("Array", "map", "(a -> b) -> Array a -> Array b"),
            V("Array", "indexedMap", "(Int -> a -> b) -> Array a -> Array b"),
            V("Array", "foldl", "(a -> b -> b) -> b -> Array a -> b"),
            V("Array", "foldr", "(a -> b -> b) -> b -> Array a -> b"),
            V("Array", "filter", "(a -> Bool) -> Array a -> Array a"),
            V("Array", "append", "Array a -> Array a -> Array a"),
            V("Array", "slice", "Int -> Int -> Array a -> Array a"),
            // Bitwise
            V("Bitwise", "and", "Int -> Int -> Int"),
            V("Bitwise", "or", "Int -> Int -> Int"),
            V("Bitwise", "xor", "Int -> Int -> Int"),
            V("Bitwise", "complement", "Int -> Int"),
            V("Bitwise", "shiftLeftBy", "Int -> Int -> Int"),
            V("Bitwise", "shiftRightBy", "Int -> Int -> Int"),
            V("Bitwise", "shiftRightZfBy", "Int -> Int -> Int"),
            // Html
            V("Html", "text", "String -> Html msg"),
            V("Html", "node", "String -> List (Attribute msg) -> List (Html msg) -> Html msg"),
            V("Html", "map", "(a -> msg) -> Html a -> Html msg"),
            // Html.Attributes
            V("Html.Attributes", "style", "String -> String -> Attribute msg"),
            V("Html.Attributes", "attribute", "String -> String -> Attribute msg"),
            V("Html.Attributes", "map", "(a -> msg) -> Attribute a -> Attribute msg"),
            // Html.Events
            V("Html.Events", "onClick", "msg -> Attribute msg"),
            V("Html.Events", "onDoubleClick", "msg -> Attribute msg"),
            V("Html.Events", "onMouseDown", "msg -> Attribute msg"),
            V("Html.Events", "onMouseUp", "msg -> Attribute msg"),
            V("Html.Events", "onMouseEnter", "msg -> Attribute msg"),
            V("Html.Events", "onMouseLeave", "msg -> Attribute msg"),
            V("Html.Events", "onMouseOver", "msg -> Attribute msg"),
            V("Html.Events", "onMouseOut", "msg -> Attribute msg"),
            V("Html.Events", "onInput", "(String -> msg) -> Attribute msg"),
            V("Html.Events", "onCheck", "(Bool -> msg) -> Attribute msg"),
            V("Html.Events", "onSubmit", "msg -> Attribute msg"),
            // Browser
            V(
                "Browser",
                "sandbox",
                "{ init : model, update : msg -> model -> model, view : model -> Html msg } -> Program () model msg",
            ),
            V(
                "Browser",
                "document",
                "{ init : flags -> ( model, Cmd msg ), update : msg -> model -> ( model, Cmd msg ), subscriptions : model -> Sub msg, view : model -> Browser.Document msg } -> Program flags model msg",
            ),
            V(
                "Browser",
                "application",
                "{ init : flags -> Url.Url -> Browser.Navigation.Key -> ( model, Cmd msg ), update : msg -> model -> ( model, Cmd msg ), subscriptions : model -> Sub msg, view : model -> Browser.Document msg, onUrlRequest : Browser.UrlRequest -> msg, onUrlChange : Url.Url -> msg } -> Program flags model msg",
            ),
            V(
                "Browser",
                "element",
                "{ init : flags -> ( model, Cmd msg ), update : msg -> model -> ( model, Cmd msg ), subscriptions : model -> Sub msg, view : model -> Html msg } -> Program flags model msg",
            ),
            // Platform
            V(
                "Platform",
                "worker",
                "{ init : flags -> ( model, Cmd msg ), update : msg -> model -> ( model, Cmd msg ), subscriptions : model -> Sub msg } -> Program flags model msg",
            ),
            // Platform.Cmd
            V("Platform.Cmd", "none", "Cmd msg"),
            V("Platform.Cmd", "batch", "List (Cmd msg) -> Cmd msg"),
            V("Platform.Cmd", "map", "(a -> msg) -> Cmd a -> Cmd msg"),
            // Platform.Sub
            V("Platform.Sub", "none", "Sub msg"),
            V("Platform.Sub", "batch", "List (Sub msg) -> Sub msg"),
            V("Platform.Sub", "map", "(a -> msg) -> Sub a -> Sub msg"),
        ];
        for tag in HTML_TAGS {
            table.push(V(
                "Html",
                tag,
                "List (Attribute msg) -> List (Html msg) -> Html msg",
            ));
        }
        for attr in HTML_STRING_ATTRS {
            table.push(V("Html.Attributes", attr, "String -> Attribute msg"));
        }
        for attr in HTML_BOOL_ATTRS {
            table.push(V("Html.Attributes", attr, "Bool -> Attribute msg"));
        }
        for attr in HTML_INT_ATTRS {
            table.push(V("Html.Attributes", attr, "Int -> Attribute msg"));
        }
        for tag in SVG_TAGS {
            table.push(V(
                "Svg",
                tag,
                "List (Attribute msg) -> List (Html msg) -> Html msg",
            ));
        }
        for (attr, _) in SVG_ATTRS {
            table.push(V("Svg.Attributes", attr, "String -> Attribute msg"));
        }
        table.extend([
            // Svg — non-tag helpers
            V("Svg", "text", "String -> Html msg"),
            V("Svg", "map", "(a -> msg) -> Html a -> Html msg"),
            V("Svg", "node", "String -> List (Attribute msg) -> List (Svg msg) -> Svg msg"),
            V("Svg.Attributes", "clipPath", "String -> Attribute msg"),
            // Html — extras
            V("Html.Attributes", "classList", "List ( String, Bool ) -> Attribute msg"),
            V("Html.Attributes", "property", "String -> Value -> Attribute msg"),
            V("Html.Events", "on", "String -> Decoder msg -> Attribute msg"),
            V("Html.Events", "stopPropagationOn", "String -> Decoder ( msg, Bool ) -> Attribute msg"),
            V("Html.Events", "preventDefaultOn", "String -> Decoder ( msg, Bool ) -> Attribute msg"),
            V("Html.Events", "targetValue", "Decoder String"),
            V("Html.Events", "targetChecked", "Decoder Bool"),
            V("Html.Events", "keyCode", "Decoder Int"),
            V("Html.Events", "onBlur", "msg -> Attribute msg"),
            V("Html.Events", "custom", "String -> Decoder { message : msg, stopPropagation : Bool, preventDefault : Bool } -> Attribute msg"),
            V("Html.Events", "onFocus", "msg -> Attribute msg"),
            V("Html.Lazy", "lazy", "(a -> Html msg) -> a -> Html msg"),
            V("Html.Lazy", "lazy2", "(a -> b -> Html msg) -> a -> b -> Html msg"),
            V("Html.Lazy", "lazy3", "(a -> b -> c -> Html msg) -> a -> b -> c -> Html msg"),
            V("Html.Lazy", "lazy4", "(a -> b -> c -> d -> Html msg) -> a -> b -> c -> d -> Html msg"),
            V("Html.Lazy", "lazy5", "(a -> b -> c -> d -> e -> Html msg) -> a -> b -> c -> d -> e -> Html msg"),
            V("Html.Lazy", "lazy6", "(a -> b -> c -> d -> e -> f -> Html msg) -> a -> b -> c -> d -> e -> f -> Html msg"),
            V("Html.Lazy", "lazy7", "(a -> b -> c -> d -> e -> f -> g -> Html msg) -> a -> b -> c -> d -> e -> f -> g -> Html msg"),
            V("Html.Lazy", "lazy8", "(a -> b -> c -> d -> e -> f -> g -> h -> Html msg) -> a -> b -> c -> d -> e -> f -> g -> h -> Html msg"),
            V("Html.Keyed", "node", "String -> List (Attribute msg) -> List ( String, Html msg ) -> Html msg"),
            V("Html.Keyed", "ul", "List (Attribute msg) -> List ( String, Html msg ) -> Html msg"),
            V("Html.Keyed", "ol", "List (Attribute msg) -> List ( String, Html msg ) -> Html msg"),
            // Json.Decode
            V("Json.Decode", "decodeString", "Decoder a -> String -> Result Json.Decode.Error a"),
            V("Json.Decode", "decodeValue", "Decoder a -> Value -> Result Json.Decode.Error a"),
            V("Json.Decode", "errorToString", "Json.Decode.Error -> String"),
            V("Json.Decode", "string", "Decoder String"),
            V("Json.Decode", "int", "Decoder Int"),
            V("Json.Decode", "float", "Decoder Float"),
            V("Json.Decode", "bool", "Decoder Bool"),
            V("Json.Decode", "value", "Decoder Value"),
            V("Json.Decode", "null", "a -> Decoder a"),
            V("Json.Decode", "nullable", "Decoder a -> Decoder (Maybe a)"),
            V("Json.Decode", "list", "Decoder a -> Decoder (List a)"),
            V("Json.Decode", "array", "Decoder a -> Decoder (Array a)"),
            V("Json.Decode", "dict", "Decoder a -> Decoder (Dict String a)"),
            V("Json.Decode", "keyValuePairs", "Decoder a -> Decoder (List ( String, a ))"),
            V("Json.Decode", "field", "String -> Decoder a -> Decoder a"),
            V("Json.Decode", "at", "List String -> Decoder a -> Decoder a"),
            V("Json.Decode", "index", "Int -> Decoder a -> Decoder a"),
            V("Json.Decode", "maybe", "Decoder a -> Decoder (Maybe a)"),
            V("Json.Decode", "oneOf", "List (Decoder a) -> Decoder a"),
            V("Json.Decode", "lazy", "(() -> Decoder a) -> Decoder a"),
            V("Json.Decode", "map", "(a -> value) -> Decoder a -> Decoder value"),
            V("Json.Decode", "map2", "(a -> b -> value) -> Decoder a -> Decoder b -> Decoder value"),
            V("Json.Decode", "map3", "(a -> b -> c -> value) -> Decoder a -> Decoder b -> Decoder c -> Decoder value"),
            V("Json.Decode", "map4", "(a -> b -> c -> d -> value) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder value"),
            V("Json.Decode", "map5", "(a -> b -> c -> d -> e -> value) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder e -> Decoder value"),
            V("Json.Decode", "map6", "(a -> b -> c -> d -> e -> f -> value) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder e -> Decoder f -> Decoder value"),
            V("Json.Decode", "map7", "(a -> b -> c -> d -> e -> f -> g -> value) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder e -> Decoder f -> Decoder g -> Decoder value"),
            V("Json.Decode", "map8", "(a -> b -> c -> d -> e -> f -> g -> h -> value) -> Decoder a -> Decoder b -> Decoder c -> Decoder d -> Decoder e -> Decoder f -> Decoder g -> Decoder h -> Decoder value"),
            V("Json.Decode", "andThen", "(a -> Decoder b) -> Decoder a -> Decoder b"),
            V("Json.Decode", "succeed", "a -> Decoder a"),
            V("Json.Decode", "fail", "String -> Decoder a"),
            // Json.Encode
            V("Json.Encode", "encode", "Int -> Value -> String"),
            V("Json.Encode", "string", "String -> Value"),
            V("Json.Encode", "int", "Int -> Value"),
            V("Json.Encode", "float", "Float -> Value"),
            V("Json.Encode", "bool", "Bool -> Value"),
            V("Json.Encode", "null", "Value"),
            V("Json.Encode", "list", "(a -> Value) -> List a -> Value"),
            V("Json.Encode", "array", "(a -> Value) -> Array a -> Value"),
            V("Json.Encode", "object", "List ( String, Value ) -> Value"),
            V("Json.Encode", "set", "(a -> Value) -> Set a -> Value"),
            V("Json.Encode", "dict", "(k -> String) -> (v -> Value) -> Dict k v -> Value"),
            // Task
            V("Task", "perform", "(a -> msg) -> Task Never a -> Cmd msg"),
            V("Task", "attempt", "(Result x a -> msg) -> Task x a -> Cmd msg"),
            V("Task", "succeed", "a -> Task x a"),
            V("Task", "fail", "x -> Task x a"),
            V("Task", "map", "(a -> b) -> Task x a -> Task x b"),
            V("Task", "map2", "(a -> b -> result) -> Task x a -> Task x b -> Task x result"),
            V("Task", "andThen", "(a -> Task x b) -> Task x a -> Task x b"),
            V("Task", "onError", "(x -> Task y a) -> Task x a -> Task y a"),
            V("Task", "mapError", "(x -> y) -> Task x a -> Task y a"),
            V("Task", "sequence", "List (Task x a) -> Task x (List a)"),
            // Process
            V("Process", "sleep", "Float -> Task x ()"),
            // Terminal — server-side output (the start of the native
            // platform surface; also works in the JS backend via console).
            V("Terminal", "writeLine", "String -> Cmd msg"),
            // Time
            V("Time", "now", "Task x Time.Posix"),
            V("Time", "posixToMillis", "Time.Posix -> Int"),
            V("Time", "millisToPosix", "Int -> Time.Posix"),
            V("Time", "utc", "Time.Zone"),
            V("Time", "here", "Task x Time.Zone"),
            V("Time", "customZone", "Int -> List { start : Int, offset : Int } -> Time.Zone"),
            V("Time", "every", "Float -> (Time.Posix -> msg) -> Sub msg"),
            V("Time", "toYear", "Time.Zone -> Time.Posix -> Int"),
            V("Time", "toMonth", "Time.Zone -> Time.Posix -> Time.Month"),
            V("Time", "toDay", "Time.Zone -> Time.Posix -> Int"),
            V("Time", "toHour", "Time.Zone -> Time.Posix -> Int"),
            V("Time", "toMinute", "Time.Zone -> Time.Posix -> Int"),
            V("Time", "toSecond", "Time.Zone -> Time.Posix -> Int"),
            V("Time", "toMillis", "Time.Zone -> Time.Posix -> Int"),
            V("Time", "toWeekday", "Time.Zone -> Time.Posix -> Time.Weekday"),
            V("Time", "getZoneName", "Task x Time.ZoneName"),
            // Http
            V("Http", "get", "{ url : String, expect : Http.Expect msg } -> Cmd msg"),
            V("Http", "post", "{ url : String, body : Http.Body, expect : Http.Expect msg } -> Cmd msg"),
            V("Http", "request", "{ method : String, headers : List Http.Header, url : String, body : Http.Body, expect : Http.Expect msg, timeout : Maybe Float, tracker : Maybe String } -> Cmd msg"),
            V("Http", "riskyRequest", "{ method : String, headers : List Http.Header, url : String, body : Http.Body, expect : Http.Expect msg, timeout : Maybe Float, tracker : Maybe String } -> Cmd msg"),
            V("Http", "riskyTask", "{ method : String, headers : List Http.Header, url : String, body : Http.Body, resolver : Http.Resolver x a, timeout : Maybe Float } -> Task x a"),
            V("Http", "header", "String -> String -> Http.Header"),
            V("Http", "emptyBody", "Http.Body"),
            V("Http", "jsonBody", "Value -> Http.Body"),
            V("Http", "stringBody", "String -> String -> Http.Body"),
            V("Http", "fileBody", "File -> Http.Body"),
            V("Http", "multipartBody", "List Http.Part -> Http.Body"),
            V("Http", "stringPart", "String -> String -> Http.Part"),
            V("Http", "filePart", "String -> File -> Http.Part"),
            V("Http", "expectJson", "(Result Http.Error a -> msg) -> Decoder a -> Http.Expect msg"),
            V("Http", "expectString", "(Result Http.Error String -> msg) -> Http.Expect msg"),
            V("Http", "expectWhatever", "(Result Http.Error () -> msg) -> Http.Expect msg"),
            V("Http", "expectStringResponse", "(Result x a -> msg) -> (Http.Response String -> Result x a) -> Http.Expect msg"),
            V("Http", "expectBytes", "(Result Http.Error a -> msg) -> Bytes.Decode.Decoder a -> Http.Expect msg"),
            V("Http", "expectBytesResponse", "(Result x a -> msg) -> (Http.Response Bytes.Bytes -> Result x a) -> Http.Expect msg"),
            V("Http", "bytesBody", "String -> Bytes.Bytes -> Http.Body"),
            V("Http", "bytesPart", "String -> String -> Bytes.Bytes -> Http.Part"),
            V("Http", "task", "{ method : String, headers : List Http.Header, url : String, body : Http.Body, resolver : Http.Resolver x a, timeout : Maybe Float } -> Task x a"),
            V("Http", "stringResolver", "(Http.Response String -> Result x a) -> Http.Resolver x a"),
            V("Http", "track", "String -> (Http.Progress -> msg) -> Sub msg"),
            V("Http", "fractionSent", "{ sent : Int, size : Int } -> Float"),
            V("Http", "fractionReceived", "{ received : Int, size : Maybe Int } -> Float"),
            V("Http", "cancel", "String -> Cmd msg"),
            // File
            V("File", "decoder", "Decoder File"),
            V("File", "name", "File -> String"),
            V("File", "size", "File -> Int"),
            V("File", "mime", "File -> String"),
            // Url
            V("Url", "fromString", "String -> Maybe Url.Url"),
            V("Url", "toString", "Url.Url -> String"),
            V("Url", "percentEncode", "String -> String"),
            V("Url", "percentDecode", "String -> Maybe String"),
            // Browser.Dom
            V("Browser.Dom", "focus", "String -> Task Browser.Dom.Error ()"),
            V("Browser.Dom", "blur", "String -> Task Browser.Dom.Error ()"),
            V("Browser.Dom", "getViewport", "Task x Browser.Dom.Viewport"),
            V("Browser.Dom", "setViewport", "Float -> Float -> Task x ()"),
            V("Browser.Dom", "getElement", "String -> Task Browser.Dom.Error Browser.Dom.Element"),
            V("Browser.Dom", "getViewportOf", "String -> Task Browser.Dom.Error Browser.Dom.Viewport"),
            V("Browser.Dom", "setViewportOf", "String -> Float -> Float -> Task Browser.Dom.Error ()"),
            // Browser.Events
            V("Browser.Events", "onKeyDown", "Decoder msg -> Sub msg"),
            V("Browser.Events", "onKeyUp", "Decoder msg -> Sub msg"),
            V("Browser.Events", "onKeyPress", "Decoder msg -> Sub msg"),
            V("Browser.Events", "onClick", "Decoder msg -> Sub msg"),
            V("Browser.Events", "onMouseMove", "Decoder msg -> Sub msg"),
            V("Browser.Events", "onMouseDown", "Decoder msg -> Sub msg"),
            V("Browser.Events", "onMouseUp", "Decoder msg -> Sub msg"),
            V("Browser.Events", "onResize", "(Int -> Int -> msg) -> Sub msg"),
            V("Browser.Events", "onAnimationFrameDelta", "(Float -> msg) -> Sub msg"),
            V("Browser.Events", "onAnimationFrame", "(Time.Posix -> msg) -> Sub msg"),
            // Browser.Navigation
            V("Browser.Navigation", "load", "String -> Cmd msg"),
            V("Browser.Navigation", "reload", "Cmd msg"),
            V("Browser.Navigation", "pushUrl", "Browser.Navigation.Key -> String -> Cmd msg"),
            V("Browser.Navigation", "replaceUrl", "Browser.Navigation.Key -> String -> Cmd msg"),
            V("Browser.Navigation", "back", "Browser.Navigation.Key -> Int -> Cmd msg"),
            V("Browser.Navigation", "forward", "Browser.Navigation.Key -> Int -> Cmd msg"),
            // Random (the effect-module parts reimplemented natively)
            V("Random", "int", "Int -> Int -> Random.Generator Int"),
            V("Random", "float", "Float -> Float -> Random.Generator Float"),
            V("Random", "constant", "a -> Random.Generator a"),
            V("Random", "map", "(a -> b) -> Random.Generator a -> Random.Generator b"),
            V("Random", "map2", "(a -> b -> c) -> Random.Generator a -> Random.Generator b -> Random.Generator c"),
            V("Random", "map3", "(a -> b -> c -> d) -> Random.Generator a -> Random.Generator b -> Random.Generator c -> Random.Generator d"),
            V("Random", "map4", "(a -> b -> c -> d -> e) -> Random.Generator a -> Random.Generator b -> Random.Generator c -> Random.Generator d -> Random.Generator e"),
            V("Random", "map5", "(a -> b -> c -> d -> e -> f) -> Random.Generator a -> Random.Generator b -> Random.Generator c -> Random.Generator d -> Random.Generator e -> Random.Generator f"),
            V("Random", "andThen", "(a -> Random.Generator b) -> Random.Generator a -> Random.Generator b"),
            V("Random", "lazy", "(() -> Random.Generator a) -> Random.Generator a"),
            V("Random", "list", "Int -> Random.Generator a -> Random.Generator (List a)"),
            V("Random", "pair", "Random.Generator a -> Random.Generator b -> Random.Generator ( a, b )"),
            V("Random", "uniform", "a -> List a -> Random.Generator a"),
            V("Random", "weighted", "( Float, a ) -> List ( Float, a ) -> Random.Generator a"),
            V("Random", "step", "Random.Generator a -> Random.Seed -> ( a, Random.Seed )"),
            V("Random", "initialSeed", "Int -> Random.Seed"),
            V("Random", "independentSeed", "Random.Generator Random.Seed"),
            V("Random", "generate", "(a -> msg) -> Random.Generator a -> Cmd msg"),
            V("Random", "minInt", "Int"),
            V("Random", "maxInt", "Int"),
            // UUID (TSFoster/elm-uuid surface used in practice)
            V("UUID", "generator", "Random.Generator UUID.UUID"),
            V("UUID", "toString", "UUID.UUID -> String"),
            V("UUID", "compare", "UUID.UUID -> UUID.UUID -> Order"),
            V("UUID", "toRepresentation", "UUID.Representation -> UUID.UUID -> String"),
            V("UUID", "fromString", "String -> Result UUID.Error UUID.UUID"),
            V("UUID", "jsonDecoder", "Decoder UUID.UUID"),
            V("UUID", "toValue", "UUID.UUID -> Value"),
            // VirtualDom (the parts packages use directly)
            V("VirtualDom", "text", "String -> Html msg"),
            V("VirtualDom", "node", "String -> List (Attribute msg) -> List (Html msg) -> Html msg"),
            V("VirtualDom", "nodeNS", "String -> String -> List (Attribute msg) -> List (Html msg) -> Html msg"),
            V("VirtualDom", "attribute", "String -> String -> Attribute msg"),
            V("VirtualDom", "property", "String -> Value -> Attribute msg"),
            V("VirtualDom", "style", "String -> String -> Attribute msg"),
            V("VirtualDom", "map", "(a -> msg) -> Html a -> Html msg"),
            V("VirtualDom", "mapAttribute", "(a -> b) -> Attribute a -> Attribute b"),
            V("VirtualDom", "keyedNode", "String -> List (Attribute msg) -> List ( String, Html msg ) -> Html msg"),
            V("VirtualDom", "keyedNodeNS", "String -> String -> List (Attribute msg) -> List ( String, Html msg ) -> Html msg"),
            V("VirtualDom", "attributeNS", "String -> String -> String -> Attribute msg"),
            V("VirtualDom", "on", "String -> Handler msg -> Attribute msg"),
            V("VirtualDom", "lazy", "(a -> Html msg) -> a -> Html msg"),
            V("VirtualDom", "lazy2", "(a -> b -> Html msg) -> a -> b -> Html msg"),
            V("VirtualDom", "lazy3", "(a -> b -> c -> Html msg) -> a -> b -> c -> Html msg"),
            V("VirtualDom", "lazy4", "(a -> b -> c -> d -> Html msg) -> a -> b -> c -> d -> Html msg"),
            V("VirtualDom", "lazy5", "(a -> b -> c -> d -> e -> Html msg) -> a -> b -> c -> d -> e -> Html msg"),
            V("VirtualDom", "lazy6", "(a -> b -> c -> d -> e -> f -> Html msg) -> a -> b -> c -> d -> e -> f -> Html msg"),
            V("VirtualDom", "lazy7", "(a -> b -> c -> d -> e -> f -> g -> Html msg) -> a -> b -> c -> d -> e -> f -> g -> Html msg"),
            V("VirtualDom", "lazy8", "(a -> b -> c -> d -> e -> f -> g -> h -> Html msg) -> a -> b -> c -> d -> e -> f -> g -> h -> Html msg"),
        ]);
        table
    })
}

/// Int-valued HTML attribute helpers in Html.Attributes.
pub const HTML_INT_ATTRS: &[&str] = &[
    "rows", "cols", "colspan", "rowspan", "tabindex", "size", "maxlength", "minlength",
    "height", "width",
];

/// SVG element helpers.
pub const SVG_TAGS: &[&str] = &[
    "svg", "circle", "ellipse", "line", "path", "polygon", "polyline", "rect", "g", "defs",
    "text_", "tspan", "use", "mask", "clipPath", "linearGradient", "radialGradient", "stop",
    "pattern", "marker", "symbol", "title", "desc", "foreignObject", "animate", "a",
    "animateTransform", "image", "switch", "view", "filter", "feGaussianBlur", "feColorMatrix",
    "feOffset", "feMerge", "feMergeNode", "feBlend", "feFlood", "textPath", "style", "metadata",
];

/// SVG attribute helpers: (Elm name, DOM attribute name).
pub const SVG_ATTRS: &[(&str, &str)] = &[
    ("viewBox", "viewBox"),
    ("width", "width"),
    ("height", "height"),
    ("x", "x"),
    ("y", "y"),
    ("x1", "x1"),
    ("y1", "y1"),
    ("x2", "x2"),
    ("y2", "y2"),
    ("cx", "cx"),
    ("cy", "cy"),
    ("fx", "fx"),
    ("fy", "fy"),
    ("r", "r"),
    ("fr", "fr"),
    ("rx", "rx"),
    ("ry", "ry"),
    ("d", "d"),
    ("points", "points"),
    ("fill", "fill"),
    ("fillRule", "fill-rule"),
    ("fillOpacity", "fill-opacity"),
    ("stroke", "stroke"),
    ("strokeWidth", "stroke-width"),
    ("strokeLinecap", "stroke-linecap"),
    ("strokeLinejoin", "stroke-linejoin"),
    ("strokeDasharray", "stroke-dasharray"),
    ("strokeDashoffset", "stroke-dashoffset"),
    ("strokeOpacity", "stroke-opacity"),
    ("strokeMiterlimit", "stroke-miterlimit"),
    ("opacity", "opacity"),
    ("transform", "transform"),
    ("class", "class"),
    ("id", "id"),
    ("style", "style"),
    ("dx", "dx"),
    ("dy", "dy"),
    ("fontSize", "font-size"),
    ("fontFamily", "font-family"),
    ("textAnchor", "text-anchor"),
    ("dominantBaseline", "dominant-baseline"),
    ("gradientUnits", "gradientUnits"),
    ("gradientTransform", "gradientTransform"),
    ("offset", "offset"),
    ("stopColor", "stop-color"),
    ("stopOpacity", "stop-opacity"),
    ("clipRule", "clip-rule"),
    ("clipPathUnits", "clipPathUnits"),
    ("preserveAspectRatio", "preserveAspectRatio"),
    ("xlinkHref", "xlink:href"),
    ("pointerEvents", "pointer-events"),
    ("visibility", "visibility"),
    ("version", "version"),
    ("attributeName", "attributeName"),
    ("values", "values"),
    ("dur", "dur"),
    ("repeatCount", "repeatCount"),
    ("xmlSpace", "xml:space"),
    ("xmlLang", "xml:lang"),
    ("baseProfile", "baseProfile"),
    ("markerEnd", "marker-end"),
    ("markerStart", "marker-start"),
    ("maskUnits", "maskUnits"),
    ("maskContentUnits", "maskContentUnits"),
    ("patternContentUnits", "patternContentUnits"),
    ("patternTransform", "patternTransform"),
    ("patternUnits", "patternUnits"),
    ("vectorEffect", "vector-effect"),
    ("mask", "mask"),
    ("filter", "filter"),
    ("result", "result"),
    ("in_", "in"),
    ("in2", "in2"),
    ("mode", "mode"),
    ("stdDeviation", "stdDeviation"),
    ("floodColor", "flood-color"),
    ("floodOpacity", "flood-opacity"),
    ("spreadMethod", "spreadMethod"),
    ("href", "href"),
    ("target", "target"),
    ("cursor", "cursor"),
    ("display", "display"),
    ("overflow", "overflow"),
    ("color", "color"),
    ("fontWeight", "font-weight"),
    ("fontStyle", "font-style"),
    ("letterSpacing", "letter-spacing"),
    ("wordSpacing", "word-spacing"),
    ("textDecoration", "text-decoration"),
    ("alignmentBaseline", "alignment-baseline"),
    ("baselineShift", "baseline-shift"),
    // Animation (<animate>, <animateTransform>) attributes.
    ("keyTimes", "keyTimes"),
    ("keySplines", "keySplines"),
    ("calcMode", "calcMode"),
    ("begin", "begin"),
    ("end", "end"),
    ("from", "from"),
    ("to", "to"),
    ("by", "by"),
    ("repeatDur", "repeatDur"),
    ("additive", "additive"),
    ("accumulate", "accumulate"),
    ("restart", "restart"),
    ("attributeType", "attributeType"),
    ("type_", "type"),
];

/// The standard HTML element helpers, all `List (Attribute msg) ->
/// List (Html msg) -> Html msg`. Generated from a name list to keep the
/// signature table readable.
pub const HTML_TAGS: &[&str] = &[
    "div", "span", "p", "a", "img", "br", "hr", "pre", "code", "em", "strong", "i", "b", "u",
    "sub", "sup", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li", "dl", "dt", "dd",
    "table", "caption", "thead", "tbody", "tfoot", "tr", "td", "th", "form", "fieldset",
    "legend", "label", "input", "textarea", "button", "select", "option", "section", "header",
    "footer", "nav", "article", "aside", "main_", "figure", "figcaption", "blockquote",
    "iframe", "canvas", "audio", "video", "source", "small", "cite", "details", "summary",
    "abbr", "address", "mark", "meter", "progress", "output", "datalist", "optgroup",
    "s", "q", "del", "ins", "col", "colgroup", "track", "embed", "object", "param",
    "math", "dfn", "time", "var", "samp", "kbd", "ruby", "rt", "rp", "bdi", "bdo",
    "wbr", "menu", "menuitem",
];

/// String-valued HTML attribute helpers in Html.Attributes.
pub const HTML_STRING_ATTRS: &[&str] = &[
    "class", "id", "title", "href", "src", "alt", "name", "placeholder", "value", "type_", "draggable",
    "for", "action", "method", "target", "rel", "wrap", "accept", "list",
    "max", "min", "step", "pattern", "lang", "dir",
    "download", "hreflang", "media", "ping", "usemap", "shape", "coords", "enctype",
    "datetime", "charset", "content", "httpEquiv", "poster", "kind", "srclang", "sandbox",
    "srcdoc", "manifest", "headers", "scope", "accesskey", "cite", "align", "acceptCharset",
];

/// Bool-valued HTML attribute helpers in Html.Attributes.
pub const HTML_BOOL_ATTRS: &[&str] = &[
    "checked", "selected", "disabled", "hidden", "readonly", "required", "autofocus", "contenteditable",
    "autoplay", "controls", "loop", "multiple", "novalidate", "spellcheck", "autocomplete",
    "ismap", "default",
];

// INFIX OPERATORS — the table from elm/core's Basics.elm and List.elm.

pub struct BuiltinInfix {
    pub op: &'static str,
    pub associativity: Associativity,
    pub precedence: u8,
    pub module: &'static str,
    pub function: &'static str,
}

pub const INFIXES: &[BuiltinInfix] = &[
    BuiltinInfix { op: "<|", associativity: Associativity::Right, precedence: 0, module: "Basics", function: "apL" },
    BuiltinInfix { op: "|>", associativity: Associativity::Left, precedence: 0, module: "Basics", function: "apR" },
    BuiltinInfix { op: "||", associativity: Associativity::Right, precedence: 2, module: "Basics", function: "or" },
    BuiltinInfix { op: "&&", associativity: Associativity::Right, precedence: 3, module: "Basics", function: "and" },
    BuiltinInfix { op: "==", associativity: Associativity::Non, precedence: 4, module: "Basics", function: "eq" },
    BuiltinInfix { op: "/=", associativity: Associativity::Non, precedence: 4, module: "Basics", function: "neq" },
    BuiltinInfix { op: "<", associativity: Associativity::Non, precedence: 4, module: "Basics", function: "lt" },
    BuiltinInfix { op: ">", associativity: Associativity::Non, precedence: 4, module: "Basics", function: "gt" },
    BuiltinInfix { op: "<=", associativity: Associativity::Non, precedence: 4, module: "Basics", function: "le" },
    BuiltinInfix { op: ">=", associativity: Associativity::Non, precedence: 4, module: "Basics", function: "ge" },
    BuiltinInfix { op: "++", associativity: Associativity::Right, precedence: 5, module: "Basics", function: "append" },
    BuiltinInfix { op: "::", associativity: Associativity::Right, precedence: 5, module: "List", function: "cons" },
    BuiltinInfix { op: "+", associativity: Associativity::Left, precedence: 6, module: "Basics", function: "add" },
    BuiltinInfix { op: "-", associativity: Associativity::Left, precedence: 6, module: "Basics", function: "sub" },
    BuiltinInfix { op: "*", associativity: Associativity::Left, precedence: 7, module: "Basics", function: "mul" },
    BuiltinInfix { op: "/", associativity: Associativity::Left, precedence: 7, module: "Basics", function: "fdiv" },
    BuiltinInfix { op: "//", associativity: Associativity::Left, precedence: 7, module: "Basics", function: "idiv" },
    BuiltinInfix { op: "^", associativity: Associativity::Right, precedence: 8, module: "Basics", function: "pow" },
    BuiltinInfix { op: "<<", associativity: Associativity::Right, precedence: 9, module: "Basics", function: "composeL" },
    BuiltinInfix { op: ">>", associativity: Associativity::Left, precedence: 9, module: "Basics", function: "composeR" },
];

pub fn lookup_infix(op: &str) -> Option<&'static BuiltinInfix> {
    INFIXES.iter().find(|i| i.op == op)
}

// UNIONS

pub struct BuiltinUnion {
    pub module: &'static str,
    pub name: &'static str,
    pub vars: &'static [&'static str],
    /// (constructor name, argument type signatures)
    pub ctors: &'static [(&'static str, &'static [&'static str])],
}

pub const UNIONS: &[BuiltinUnion] = &[
    BuiltinUnion { module: "Basics", name: "Bool", vars: &[], ctors: &[("True", &[]), ("False", &[])] },
    BuiltinUnion { module: "Basics", name: "Order", vars: &[], ctors: &[("LT", &[]), ("EQ", &[]), ("GT", &[])] },
    BuiltinUnion { module: "Maybe", name: "Maybe", vars: &["a"], ctors: &[("Just", &["a"]), ("Nothing", &[])] },
    BuiltinUnion { module: "Result", name: "Result", vars: &["error", "value"], ctors: &[("Ok", &["value"]), ("Err", &["error"])] },
    BuiltinUnion { module: "Http", name: "Error", vars: &[], ctors: &[
        ("BadUrl", &["String"]),
        ("Timeout", &[]),
        ("NetworkError", &[]),
        ("BadStatus", &["Int"]),
        ("BadBody", &["String"]),
    ] },
    BuiltinUnion { module: "Http", name: "Progress", vars: &[], ctors: &[
        ("Sending", &["{ sent : Int, size : Int }"]),
        ("Receiving", &["{ received : Int, size : Maybe Int }"]),
    ] },
    BuiltinUnion { module: "Http", name: "Response", vars: &["body"], ctors: &[
        ("BadUrl_", &["String"]),
        ("Timeout_", &[]),
        ("NetworkError_", &[]),
        ("BadStatus_", &["Http.Metadata", "body"]),
        ("GoodStatus_", &["Http.Metadata", "body"]),
    ] },
    BuiltinUnion { module: "Time", name: "Month", vars: &[], ctors: &[
        ("Jan", &[]), ("Feb", &[]), ("Mar", &[]), ("Apr", &[]), ("May", &[]), ("Jun", &[]),
        ("Jul", &[]), ("Aug", &[]), ("Sep", &[]), ("Oct", &[]), ("Nov", &[]), ("Dec", &[]),
    ] },
    BuiltinUnion { module: "Time", name: "ZoneName", vars: &[], ctors: &[
        ("Name", &["String"]), ("Offset", &["Int"]),
    ] },
    BuiltinUnion { module: "Time", name: "Weekday", vars: &[], ctors: &[
        ("Mon", &[]), ("Tue", &[]), ("Wed", &[]), ("Thu", &[]), ("Fri", &[]), ("Sat", &[]), ("Sun", &[]),
    ] },
    BuiltinUnion { module: "Browser.Dom", name: "Error", vars: &[], ctors: &[("NotFound", &["String"])] },
    BuiltinUnion { module: "Url", name: "Protocol", vars: &[], ctors: &[("Http", &[]), ("Https", &[])] },
    BuiltinUnion { module: "Browser", name: "UrlRequest", vars: &[], ctors: &[
        ("Internal", &["Url.Url"]), ("External", &["String"]),
    ] },
    BuiltinUnion { module: "UUID", name: "Representation", vars: &[], ctors: &[
        ("Canonical", &[]), ("Compact", &[]), ("Guid", &[]), ("Urn", &[]),
    ] },
    BuiltinUnion { module: "VirtualDom", name: "Handler", vars: &["msg"], ctors: &[
        ("Normal", &["Decoder msg"]),
        ("MayStopPropagation", &["Decoder ( msg, Bool )"]),
        ("MayPreventDefault", &["Decoder ( msg, Bool )"]),
        ("Custom", &["Decoder { message : msg, stopPropagation : Bool, preventDefault : Bool }"]),
    ] },
    BuiltinUnion { module: "Json.Decode", name: "Error", vars: &[], ctors: &[
        ("Field", &["String", "Json.Decode.Error"]),
        ("Index", &["Int", "Json.Decode.Error"]),
        ("OneOf", &["List Json.Decode.Error"]),
        ("Failure", &["String", "Value"]),
    ] },
];

/// Built-in type aliases: (module, name, vars, body signature).
pub const ALIASES: &[(&str, &str, &[&str], &str)] = &[
    ("Json.Decode", "Value", &[], "Value"),
    ("Http", "Metadata", &[], "{ url : String, statusCode : Int, statusText : String, headers : Dict String String }"),
    ("Browser.Dom", "Viewport", &[], "{ scene : { width : Float, height : Float }, viewport : { x : Float, y : Float, width : Float, height : Float } }"),
    ("Browser.Dom", "Element", &[], "{ scene : { width : Float, height : Float }, viewport : { x : Float, y : Float, width : Float, height : Float }, element : { x : Float, y : Float, width : Float, height : Float } }"),
    ("Url", "Url", &[], "{ protocol : Protocol, host : String, port_ : Maybe Int, path : String, query : Maybe String, fragment : Maybe String }"),
    ("Svg", "Svg", &["msg"], "Html msg"),
    ("Svg", "Attribute", &["msg"], "Attribute msg"),
    ("VirtualDom", "Node", &["msg"], "Html msg"),
    ("VirtualDom", "Attribute", &["msg"], "Attribute msg"),
    ("Browser", "Document", &["msg"], "{ title : String, body : List (Html msg) }"),
    // `Platform` re-exposes the Task type (`import Platform exposing (Task)`).
    ("Platform", "Task", &["err", "ok"], "Task err ok"),
];

pub fn lookup_alias(module: &str, name: &str) -> Option<(&'static [&'static str], &'static str)> {
    ALIASES
        .iter()
        .find(|(m, n, _, _)| *m == module && *n == name)
        .map(|(_, _, vars, body)| (*vars, *body))
}

/// Types that live in builtin modules, addressed by (module, name). Used
/// for qualified type resolution where bare names would be ambiguous
/// (e.g. Http.Error vs Json.Decode.Error).
pub fn is_builtin_type(module: &str, name: &str) -> bool {
    if lookup_type_home(name) == Some(module) {
        return true;
    }
    UNIONS
        .iter()
        .any(|u| u.module == module && u.name == name)
        || OPAQUE_TYPES.contains(&(module, name))
}

/// Opaque built-in types (no exposed constructors or alias body): the ones a
/// module hands out abstractly. Kept as a table so `exposing (..)` on a
/// builtin module can bring them into scope alongside its unions and aliases.
pub const OPAQUE_TYPES: &[(&str, &str)] = &[
    ("Http", "Expect"),
    ("Http", "Body"),
    ("Http", "Header"),
    ("Http", "Part"),
    ("Http", "Resolver"),
    ("Time", "Posix"),
    ("Time", "Zone"),
    ("Time", "ZoneName"),
    ("Task", "Task"),
    ("Json.Decode", "Decoder"),
    ("Json.Encode", "Value"),
    ("File", "File"),
    ("Html", "Html"),
    ("Html", "Attribute"),
    ("Platform", "Program"),
    ("Platform.Cmd", "Cmd"),
    ("Platform.Sub", "Sub"),
    ("Random", "Generator"),
    ("Random", "Seed"),
    ("Browser.Navigation", "Key"),
    ("UUID", "UUID"),
    ("UUID", "Error"),
];

/// Where each built-in type constructor lives.
pub fn lookup_type_home(name: &str) -> Option<&'static str> {
    match name {
        "Int" | "Float" | "Bool" | "Order" | "Never" => Some("Basics"),
        "String" => Some("String"),
        "Char" => Some("Char"),
        "List" => Some("List"),
        "Maybe" => Some("Maybe"),
        "Result" => Some("Result"),
        "Dict" => Some("Dict"),
        "Set" => Some("Set"),
        "Array" => Some("Array"),
        "Html" | "Attribute" => Some("Html"),
        "Handler" => Some("VirtualDom"),
        "Program" => Some("Platform"),
        "Cmd" => Some("Platform.Cmd"),
        "Sub" => Some("Platform.Sub"),
        "Value" => Some("Json.Encode"),
        "Decoder" => Some("Json.Decode"),
        "Posix" | "Zone" | "Month" | "Weekday" => Some("Time"),
        "Task" => Some("Task"),
        "File" => Some("File"),
        "Protocol" => Some("Url"),
        _ => None,
    }
}

/// Modules that are implicitly importable (Elm's default imports, plus the
/// core data structure modules).
pub const MODULES: &[&str] = &[
    "Basics", "List", "String", "Char", "Maybe", "Result", "Tuple", "Debug", "Dict", "Set",
    "Array", "Bitwise", "Html", "Html.Attributes", "Html.Events", "Html.Lazy", "Html.Keyed",
    "Browser", "Browser.Dom", "Browser.Events", "Browser.Navigation", "Platform",
    "Platform.Cmd", "Platform.Sub", "Json.Decode", "Json.Encode", "Task", "Process", "Time",
    "Http", "File", "Url", "Svg", "Svg.Attributes", "Random", "UUID", "VirtualDom", "Terminal",
];

pub fn is_builtin_module(name: &str) -> bool {
    MODULES.contains(&name)
}

/// Look up a builtin union by module and type name.
pub fn lookup_ctor_by_union(module: &str, union_name: &str) -> Option<&'static BuiltinUnion> {
    UNIONS
        .iter()
        .find(|u| u.module == module && u.name == union_name)
}

pub fn lookup_value(module: &str, name: &str) -> Option<&'static BuiltinValue> {
    static INDEX: std::sync::OnceLock<
        std::collections::HashMap<(&'static str, &'static str), &'static BuiltinValue>,
    > = std::sync::OnceLock::new();
    INDEX
        .get_or_init(|| values().iter().map(|v| ((v.module, v.name), v)).collect())
        .get(&(module, name))
        .copied()
}

/// Values exposed unqualified by the default imports (`Basics exposing (..)`).
pub fn lookup_exposed_value(name: &str) -> Option<&'static BuiltinValue> {
    // Only user-facing Basics names are exposed; the operator functions
    // (add, apL, ...) are reachable through their operators instead.
    lookup_value("Basics", name)
}

/// Constructors exposed unqualified by the default imports:
/// Bool and Order (Basics), Maybe(..), and Result(..).
pub fn lookup_exposed_ctor(name: &str) -> Option<(&'static BuiltinUnion, u32)> {
    lookup_qualified_ctor_in(&["Basics", "Maybe", "Result"], name)
}

pub fn lookup_ctor(module: &str, name: &str) -> Option<(&'static BuiltinUnion, u32)> {
    UNIONS
        .iter()
        .filter(|u| u.module == module)
        .find_map(|u| find_ctor(u, name))
}

fn lookup_qualified_ctor_in(
    modules: &[&str],
    name: &str,
) -> Option<(&'static BuiltinUnion, u32)> {
    UNIONS
        .iter()
        .filter(|u| modules.contains(&u.module))
        .find_map(|u| find_ctor(u, name))
}

fn find_ctor(union: &'static BuiltinUnion, name: &str) -> Option<(&'static BuiltinUnion, u32)> {
    union
        .ctors
        .iter()
        .position(|(ctor, _)| *ctor == name)
        .map(|i| (union, i as u32))
}

// SIGNATURE PARSING

/// Parse a built-in type signature into a canonical type. Panics on
/// malformed signatures — they are compiled into the binary, so a failure
/// is an alm bug, not a user error.
///
/// Results are cached: the type checker resolves the same signatures at
/// every use site of every builtin. (Thread-local because `Name` is
/// `Rc`-backed; the compiler is single-threaded.)
pub fn parse_signature(signature: &str) -> Type {
    thread_local! {
        static CACHE: std::cell::RefCell<std::collections::HashMap<String, Type>> =
            std::cell::RefCell::new(std::collections::HashMap::new());
    }
    if let Some(cached) = CACHE.with(|c| c.borrow().get(signature).cloned()) {
        return cached;
    }
    let mut p = Parser::new(signature);
    let tipe = crate::parse::type_::expression(&mut p)
        .unwrap_or_else(|e| panic!("bad builtin signature {:?}: {}", signature, e.message));
    // NOTE: alias expansion recurses into parse_signature; the cache borrow
    // is released before this call.
    let result = canonicalize_signature_type(&tipe);
    CACHE.with(|c| {
        c.borrow_mut().insert(signature.to_string(), result.clone());
    });
    result
}

fn canonicalize_signature_type(tipe: &source::Type) -> Type {
    match &tipe.value {
        source::Type_::Lambda(arg, result) => Type::Lambda(
            Box::new(canonicalize_signature_type(arg)),
            Box::new(canonicalize_signature_type(result)),
        ),
        source::Type_::Var(name) => Type::Var(name.clone()),
        source::Type_::Type(_, name, args) => {
            let args: Vec<Type> = args.iter().map(canonicalize_signature_type).collect();
            if let Some(home) = lookup_type_home(name.as_str()) {
                return Type::Type(Name::from(home), name.clone(), args);
            }
            // Unambiguous builtin alias referenced without qualification.
            if let Some((_, _, vars, body)) =
                ALIASES.iter().find(|(_, n, _, _)| *n == name.as_str())
            {
                return expand_signature_alias(vars, body, args);
            }
            panic!("unknown type {} in builtin signature", name)
        }
        source::Type_::TypeQual(_, qualifier, name, args) => {
            let args: Vec<Type> = args.iter().map(canonicalize_signature_type).collect();
            if let Some((vars, body)) = lookup_alias(qualifier.as_str(), name.as_str()) {
                return expand_signature_alias(vars, body, args);
            }
            Type::Type(qualifier.clone(), name.clone(), args)
        }
        source::Type_::Record(fields, ext) => Type::Record(
            fields
                .iter()
                .map(|(name, t)| (name.value.clone(), canonicalize_signature_type(t)))
                .collect(),
            ext.as_ref().map(|n| n.value.clone()),
        ),
        source::Type_::Unit => Type::Unit,
        source::Type_::Tuple(a, b, rest) => Type::Tuple(
            Box::new(canonicalize_signature_type(a)),
            Box::new(canonicalize_signature_type(b)),
            rest.first().map(|t| Box::new(canonicalize_signature_type(t))),
        ),
    }
}

fn expand_signature_alias(vars: &[&str], body: &str, args: Vec<Type>) -> Type {
    let expanded = parse_signature(body);
    let map: std::collections::HashMap<Name, Type> = vars
        .iter()
        .map(|v| Name::from(*v))
        .zip(args)
        .collect();
    crate::canonicalize::subst_can_type(&expanded, &map)
}

