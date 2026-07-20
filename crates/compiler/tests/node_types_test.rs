//! Phase 1 of monomorphization: the checker records the concrete type of
//! every expression, keyed by source region, with variable names aligned to
//! the enclosing definition's scheme. These tests pin that behaviour down —
//! the monomorphization pass consumes exactly this map.

use alm_compiler::ast::canonical as can;
use alm_compiler::interface::Interfaces;
use alm_compiler::reporting::Region;
use std::rc::Rc;
use alm_compiler::{canonicalize, parse, typecheck};

fn check(src: &str) -> (can::Module, std::collections::HashMap<Region, can::Type>) {
    let module = parse::parse_module(src).expect("parse");
    let canonical = canonicalize::canonicalize(&module).expect("canonicalize");
    let interfaces = Interfaces::new();
    let checked = typecheck::check_module(&canonical, &interfaces).expect("check");
    (canonical, checked.node_types)
}

/// Collect every expression node together with its region, walking the
/// whole module so tests can look one up by shape.
fn collect<'a>(module: &'a can::Module, out: &mut Vec<&'a can::Expr>) {
    for group in &module.decls {
        match group {
            can::DeclGroup::Value(def) => walk(&def.body, out),
            can::DeclGroup::Recursive(defs) => {
                for def in defs {
                    walk(&def.body, out)
                }
            }
        }
    }
}

fn walk<'a>(expr: &'a can::Expr, out: &mut Vec<&'a can::Expr>) {
    use can::Expr_::*;
    out.push(expr);
    match &expr.value {
        Negate(inner) => walk(inner, out),
        List(items) => items.iter().for_each(|e| walk(e, out)),
        Binop(_, _, _, l, r) => {
            walk(l, out);
            walk(r, out)
        }
        Lambda(_, body) => walk(body, out),
        Call(f, args) => {
            walk(f, out);
            args.iter().for_each(|e| walk(e, out))
        }
        If(branches, otherwise) => {
            for (c, b) in branches {
                walk(c, out);
                walk(b, out)
            }
            walk(otherwise, out)
        }
        Let(decls, body) => {
            for decl in decls {
                match decl {
                    can::LetDecl::Def(def) => walk(&def.body, out),
                    can::LetDecl::Recursive(defs) => {
                        defs.iter().for_each(|d| walk(&d.body, out))
                    }
                    can::LetDecl::Destruct(_, value) => walk(value, out),
                }
            }
            walk(body, out)
        }
        Case(scrutinee, branches) => {
            walk(scrutinee, out);
            branches.iter().for_each(|(_, b)| walk(b, out))
        }
        Access(record, _) => walk(record, out),
        Update(record, fields) => {
            walk(record, out);
            fields.iter().for_each(|(_, v)| walk(v, out))
        }
        Record(fields) => fields.iter().for_each(|(_, v)| walk(v, out)),
        Tuple(a, b, rest) => {
            walk(a, out);
            walk(b, out);
            rest.iter().for_each(|e| walk(e, out))
        }
        _ => {}
    }
}

fn int() -> can::Type {
    can::Type::Type("Basics".into(), "Int".into(), Rc::new(vec![]))
}
fn string() -> can::Type {
    can::Type::Type("String".into(), "String".into(), Rc::new(vec![]))
}
fn lambda(a: can::Type, b: can::Type) -> can::Type {
    can::Type::Lambda(Rc::new(a), Rc::new(b))
}

/// A polymorphic top-level function's use sites are captured at their
/// concrete instantiation, not their generic scheme.
#[test]
fn concrete_instantiation_at_call_site() {
    let (module, types) = check(
        "module Test exposing (..)\n\
         \n\
         identity x = x\n\
         \n\
         useStr = identity \"hi\"\n\
         \n\
         useInt : Int\n\
         useInt = identity 5\n",
    );
    let mut nodes = Vec::new();
    collect(&module, &mut nodes);

    let mut identity_types: Vec<can::Type> = nodes
        .iter()
        .filter(|e| matches!(&e.value, can::Expr_::VarTopLevel(n) if n.as_str() == "identity"))
        .map(|e| types[&e.region].clone())
        .collect();
    identity_types.sort_by_key(|t| format!("{:?}", t));

    // One use is `identity : String -> String`, the other `Int -> Int`.
    let mut expected = vec![lambda(string(), string()), lambda(int(), int())];
    expected.sort_by_key(|t| format!("{:?}", t));
    assert_eq!(identity_types, expected);
}

/// Inside a still-generic function, a subexpression's captured type shares
/// variable names with the function's own scheme — the alignment the
/// monomorphization substitution depends on.
#[test]
fn body_node_names_align_with_scheme() {
    let (module, types) = check(
        "module Test exposing (..)\n\
         \n\
         apply f x = f x\n",
    );
    let mut nodes = Vec::new();
    collect(&module, &mut nodes);

    // The `f x` call node: its result type is a bare variable.
    let call = nodes
        .iter()
        .find(|e| matches!(&e.value, can::Expr_::Call(..)))
        .expect("call node");
    let call_ty = types[&call.region].clone();
    let result_name = match &call_ty {
        can::Type::Var(name) => name.clone(),
        other => panic!("expected the call result to be a type variable, got {:?}", other),
    };

    // The `f` reference node: its type is `<arg> -> <result>`, and the
    // result variable must be the very same name as the call's type.
    let f_ref = nodes
        .iter()
        .find(|e| matches!(&e.value, can::Expr_::VarLocal(n) if n.as_str() == "f"))
        .expect("f reference");
    match types[&f_ref.region].clone() {
        can::Type::Lambda(_, result) => match result.as_ref() {
            can::Type::Var(name) => assert_eq!(
                name, &result_name,
                "the `f` node's result var must match the call's result var"
            ),
            other => panic!("expected f's result to be a variable, got {:?}", other),
        },
        other => panic!("expected f to have a function type, got {:?}", other),
    }
}
