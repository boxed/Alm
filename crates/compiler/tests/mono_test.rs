//! Phase 2 of monomorphization: from `main`, discover the set of concrete
//! function specializations reachable through the call graph, using the
//! per-expression types the checker captured.

use std::collections::HashSet;

use alm_compiler::ast::canonical as can;
use alm_compiler::interface::Interfaces;
use alm_compiler::ir::mono::{self, TypedExpr, TypedKind, TypedLetDecl};
use alm_compiler::{canonicalize, parse, typecheck};

fn program(src: &str) -> mono::MonoProgram {
    let module = parse::parse_module(src).expect("parse");
    let canonical = canonicalize::canonicalize(&module).expect("canonicalize");
    let interfaces = Interfaces::new();
    let checked = typecheck::check_module(&canonical, &interfaces).expect("check");
    mono::specialize_program(&canonical, &checked.types, &checked.node_types)
}

/// Every type appearing anywhere in a typed body.
fn collect_types<'a>(expr: &'a TypedExpr, out: &mut Vec<&'a can::Type>) {
    out.push(&expr.tipe);
    match &expr.kind {
        TypedKind::List(items) => items.iter().for_each(|e| collect_types(e, out)),
        TypedKind::Negate(inner) => collect_types(inner, out),
        TypedKind::Binop(_, _, _, l, r) => {
            collect_types(l, out);
            collect_types(r, out)
        }
        TypedKind::Lambda(_, body) => collect_types(body, out),
        TypedKind::Call(f, args) => {
            collect_types(f, out);
            args.iter().for_each(|e| collect_types(e, out))
        }
        TypedKind::If(branches, otherwise) => {
            for (c, b) in branches {
                collect_types(c, out);
                collect_types(b, out)
            }
            collect_types(otherwise, out)
        }
        TypedKind::Let(decls, body) => {
            for decl in decls {
                collect_let_types(decl, out)
            }
            collect_types(body, out)
        }
        TypedKind::Case(scrutinee, branches) => {
            collect_types(scrutinee, out);
            branches.iter().for_each(|(_, b)| collect_types(b, out))
        }
        TypedKind::Access(record, _) => collect_types(record, out),
        TypedKind::Update(record, fields) => {
            collect_types(record, out);
            fields.iter().for_each(|(_, v)| collect_types(v, out))
        }
        TypedKind::Record(fields) => fields.iter().for_each(|(_, v)| collect_types(v, out)),
        TypedKind::Tuple(a, b, c) => {
            collect_types(a, out);
            collect_types(b, out);
            if let Some(c) = c {
                collect_types(c, out)
            }
        }
        _ => {}
    }
}

fn collect_let_types<'a>(decl: &'a TypedLetDecl, out: &mut Vec<&'a can::Type>) {
    match decl {
        TypedLetDecl::Def { body, .. } => collect_types(body, out),
        TypedLetDecl::Recursive(defs) => defs.iter().for_each(|d| collect_let_types(d, out)),
        TypedLetDecl::Destruct(_, value) => collect_types(value, out),
    }
}

/// Every mangled function name referenced by a `Global` node.
fn collect_globals(expr: &TypedExpr, out: &mut Vec<String>) {
    match &expr.kind {
        TypedKind::Global(name) => out.push(name.to_string()),
        TypedKind::List(items) => items.iter().for_each(|e| collect_globals(e, out)),
        TypedKind::Negate(inner) => collect_globals(inner, out),
        TypedKind::Binop(_, _, _, l, r) => {
            collect_globals(l, out);
            collect_globals(r, out)
        }
        TypedKind::Lambda(_, body) => collect_globals(body, out),
        TypedKind::Call(f, args) => {
            collect_globals(f, out);
            args.iter().for_each(|e| collect_globals(e, out))
        }
        TypedKind::If(branches, otherwise) => {
            for (c, b) in branches {
                collect_globals(c, out);
                collect_globals(b, out)
            }
            collect_globals(otherwise, out)
        }
        TypedKind::Let(decls, body) => {
            for decl in decls {
                collect_let_globals(decl, out)
            }
            collect_globals(body, out)
        }
        TypedKind::Case(scrutinee, branches) => {
            collect_globals(scrutinee, out);
            branches.iter().for_each(|(_, b)| collect_globals(b, out))
        }
        TypedKind::Access(record, _) => collect_globals(record, out),
        TypedKind::Update(record, fields) => {
            collect_globals(record, out);
            fields.iter().for_each(|(_, v)| collect_globals(v, out))
        }
        TypedKind::Record(fields) => fields.iter().for_each(|(_, v)| collect_globals(v, out)),
        TypedKind::Tuple(a, b, c) => {
            collect_globals(a, out);
            collect_globals(b, out);
            if let Some(c) = c {
                collect_globals(c, out)
            }
        }
        _ => {}
    }
}

fn collect_let_globals(decl: &TypedLetDecl, out: &mut Vec<String>) {
    match decl {
        TypedLetDecl::Def { body, .. } => collect_globals(body, out),
        TypedLetDecl::Recursive(defs) => defs.iter().for_each(|d| collect_let_globals(d, out)),
        TypedLetDecl::Destruct(_, value) => collect_globals(value, out),
    }
}

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
fn specialized_bodies_are_fully_concrete() {
    // Across the whole program, no node in any specialized body may retain a
    // type variable — monomorphization must have resolved everything.
    let prog = program(
        "module Test exposing (..)\n\
         \n\
         identity x = x\n\
         \n\
         wrap x = identity x\n\
         \n\
         wrapInt : Int\n\
         wrapInt = wrap 5\n\
         \n\
         main = ( wrap \"hi\", wrapInt )\n",
    );
    for f in &prog.functions {
        let mut types = Vec::new();
        collect_types(&f.body, &mut types);
        for (pattern @ _, param_ty) in &f.params {
            let _ = pattern;
            types.push(param_ty);
        }
        for tipe in types {
            assert!(
                !mentions_var(tipe),
                "function {} still has a type variable in {:?}",
                f.mangled,
                tipe
            );
        }
    }
}

#[test]
fn every_call_target_is_defined() {
    // Each mangled name referenced by a Global node must correspond to an
    // emitted specialization — the call graph is closed.
    let prog = program(
        "module Test exposing (..)\n\
         \n\
         identity x = x\n\
         \n\
         wrap x = identity x\n\
         \n\
         wrapInt : Int\n\
         wrapInt = wrap 5\n\
         \n\
         main = ( wrap \"hi\", wrapInt )\n",
    );
    let defined: HashSet<String> = prog.functions.iter().map(|f| f.mangled.to_string()).collect();
    for f in &prog.functions {
        let mut globals = Vec::new();
        collect_globals(&f.body, &mut globals);
        for g in globals {
            assert!(
                defined.contains(&g),
                "call target `{}` referenced from `{}` has no specialization",
                g,
                f.mangled
            );
        }
    }
    // Sanity: the two specializations of `identity` both exist.
    assert!(defined.contains("identity$Fn$String$String"));
    assert!(defined.contains("identity$Fn$Int$Int"));
}

fn mentions_var(tipe: &can::Type) -> bool {
    use can::Type::*;
    match tipe {
        Var(_) => true,
        Lambda(a, b) => mentions_var(a) || mentions_var(b),
        Type(_, _, args) => args.iter().any(mentions_var),
        Record(fields, _) => fields.iter().any(|(_, t)| mentions_var(t)),
        Tuple(a, b, c) => {
            mentions_var(a) || mentions_var(b) || c.as_ref().is_some_and(|c| mentions_var(c))
        }
        Unit => false,
    }
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
