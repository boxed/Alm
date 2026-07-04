//! Phase 2 of monomorphization: from `main`, discover the set of concrete
//! function specializations reachable through the call graph, using the
//! per-expression types the checker captured.

use alm_compiler::ast::canonical as can;
use alm_compiler::interface::Interfaces;
use alm_compiler::ir::mono;
use alm_compiler::{canonicalize, parse, typecheck};

/// Run the analysis on a single-module source and return `(name, printed
/// type)` for every discovered instance, sorted for stable comparison.
fn instances(src: &str) -> Vec<(String, String)> {
    let module = parse::parse_module(src).expect("parse");
    let canonical = canonicalize::canonicalize(&module).expect("canonicalize");
    let interfaces = Interfaces::new();
    let checked = typecheck::check_module(&canonical, &interfaces).expect("check");
    let set = mono::analyze(&canonical, &checked.types, &checked.node_types);
    let mut out: Vec<(String, String)> = set
        .instances
        .iter()
        .map(|i| (i.name.to_string(), render(&i.tipe)))
        .collect();
    out.sort();
    out
}

/// A compact, structural rendering of a type — enough to tell specializations
/// apart in assertions without depending on the checker's error formatting.
fn render(tipe: &can::Type) -> String {
    use can::Type::*;
    match tipe {
        Var(name) => name.to_string(),
        Lambda(a, b) => format!("({} -> {})", render(a), render(b)),
        Type(_, name, args) if args.is_empty() => name.to_string(),
        Type(_, name, args) => format!(
            "{} {}",
            name,
            args.iter().map(render).collect::<Vec<_>>().join(" ")
        ),
        Record(fields, _) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(n, t)| format!("{}:{}", n, render(t)))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Unit => "()".to_string(),
        Tuple(a, b, c) => {
            let mut parts = vec![render(a), render(b)];
            if let Some(c) = c {
                parts.push(render(c));
            }
            format!("({})", parts.join(","))
        }
    }
}

#[test]
fn single_concrete_instance() {
    // `identity` is used once, at String.
    let got = instances(
        "module Test exposing (..)\n\
         \n\
         identity x = x\n\
         \n\
         main = identity \"hi\"\n",
    );
    assert_eq!(
        got,
        vec![
            ("identity".to_string(), "(String -> String)".to_string()),
            ("main".to_string(), "String".to_string()),
        ]
    );
}

#[test]
fn one_function_two_specializations() {
    // `identity` is used at both String and Int, so it appears twice.
    let got = instances(
        "module Test exposing (..)\n\
         \n\
         identity x = x\n\
         \n\
         useInt : Int\n\
         useInt = identity 5\n\
         \n\
         main = ( identity \"hi\", useInt )\n",
    );
    assert_eq!(
        got,
        vec![
            ("identity".to_string(), "(Int -> Int)".to_string()),
            ("identity".to_string(), "(String -> String)".to_string()),
            ("main".to_string(), "(String,Int)".to_string()),
            ("useInt".to_string(), "Int".to_string()),
        ]
    );
}

#[test]
fn specialization_propagates_through_a_generic_caller() {
    // `wrap` is generic; calling it at two types must specialize both `wrap`
    // and, transitively, the `identity` it calls inside.
    let got = instances(
        "module Test exposing (..)\n\
         \n\
         identity x = x\n\
         \n\
         wrap x = identity x\n\
         \n\
         main = ( wrap \"hi\", wrapInt )\n\
         \n\
         wrapInt : Int\n\
         wrapInt = wrap 5\n",
    );
    // wrap@String -> identity@String ; wrap@Int -> identity@Int
    assert_eq!(
        got,
        vec![
            ("identity".to_string(), "(Int -> Int)".to_string()),
            ("identity".to_string(), "(String -> String)".to_string()),
            ("main".to_string(), "(String,Int)".to_string()),
            ("wrap".to_string(), "(Int -> Int)".to_string()),
            ("wrap".to_string(), "(String -> String)".to_string()),
            ("wrapInt".to_string(), "Int".to_string()),
        ]
    );
}

#[test]
fn recursive_function_specializes_once_per_type() {
    // A self-recursive function must not loop the worklist: one instance per
    // concrete type.
    let got = instances(
        "module Test exposing (..)\n\
         \n\
         double n = n + n\n\
         \n\
         sumTo n =\n\
         \x20   if n == 0 then 0 else n + sumTo (n - 1)\n\
         \n\
         main : Int\n\
         main = double (sumTo 10)\n",
    );
    assert_eq!(
        got,
        vec![
            ("double".to_string(), "(Int -> Int)".to_string()),
            ("main".to_string(), "Int".to_string()),
            ("sumTo".to_string(), "(Int -> Int)".to_string()),
        ]
    );
}
