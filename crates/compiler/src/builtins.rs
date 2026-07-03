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
        vec![
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
        ]
    })
}

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
        _ => None,
    }
}

pub fn is_builtin_module(name: &str) -> bool {
    matches!(
        name,
        "Basics" | "List" | "String" | "Char" | "Maybe" | "Result" | "Tuple" | "Debug"
    )
}

pub fn lookup_value(module: &str, name: &str) -> Option<&'static BuiltinValue> {
    values().iter().find(|v| v.module == module && v.name == name)
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
pub fn parse_signature(signature: &str) -> Type {
    let mut p = Parser::new(signature);
    let tipe = crate::parse::type_::expression(&mut p)
        .unwrap_or_else(|e| panic!("bad builtin signature {:?}: {}", signature, e.message));
    canonicalize_signature_type(&tipe)
}

fn canonicalize_signature_type(tipe: &source::Type) -> Type {
    match &tipe.value {
        source::Type_::Lambda(arg, result) => Type::Lambda(
            Box::new(canonicalize_signature_type(arg)),
            Box::new(canonicalize_signature_type(result)),
        ),
        source::Type_::Var(name) => Type::Var(name.clone()),
        source::Type_::Type(_, name, args) => {
            let home = lookup_type_home(name.as_str())
                .unwrap_or_else(|| panic!("unknown type {} in builtin signature", name));
            Type::Type(
                Name::from(home),
                name.clone(),
                args.iter().map(canonicalize_signature_type).collect(),
            )
        }
        source::Type_::TypeQual(..) => panic!("qualified types not allowed in builtin signatures"),
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

/// The arity of a builtin value, derived from its signature.
pub fn arity(signature: &str) -> u32 {
    let mut tipe = parse_signature(signature);
    let mut n = 0;
    while let Type::Lambda(_, result) = tipe {
        n += 1;
        tipe = *result;
    }
    n
}
