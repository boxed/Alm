//! Canonical-AST simplification: local, type-preserving rewrites applied after
//! type checking and before code generation, so every backend benefits.
//!
//! Passes: constant folding of literal arithmetic / string concat / comparisons,
//! `negate` of a literal, short-circuit boolean identities, and collapsing an
//! `if` with a literally-`True`/`False` condition.
//!
//! **Type-preservation invariant** (required so the WasmGC monomorphizer, which
//! types every node by its source `Region`, stays correct): a rewrite may only
//! (a) splice an existing sub-expression verbatim, keeping its own region, or
//! (b) replace a whole sub-expression with a new literal stamped with *that
//! sub-expression's* region. Both keep `node_types[region]` valid because the
//! replacement has the same type as what it replaced. Never invent a fresh
//! region. (`ALM_MISSING_NODE=1` flags any violation during a WasmGC build.)
//!
//! `ALM_NO_SIMPLIFY=1` disables the pass (safety switch / benchmark baseline).

use std::cmp::Ordering;

use crate::ast::canonical as can;
use crate::data::Name;
use crate::reporting::{Located, Region};

/// Integers this large are still exactly representable as an IEEE double, so
/// compile-time folding (done in `i128`) agrees with the JS backend's
/// double-backed `Int` arithmetic. Beyond it we must not fold.
const SAFE_INT: i128 = 1 << 53;

/// Simplify every expression in a module in place.
pub fn simplify_module(module: &mut can::Module) {
    if std::env::var("ALM_NO_SIMPLIFY").is_ok() {
        return;
    }
    for group in &mut module.decls {
        match group {
            can::DeclGroup::Value(def) => simplify(&mut def.body),
            can::DeclGroup::Recursive(defs) => {
                for def in defs {
                    simplify(&mut def.body);
                }
            }
        }
    }
}

/// Simplify `e` bottom-up: rewrite children first, then fold this node to a
/// fixed point (a fold can expose another at the same node).
fn simplify(e: &mut can::Expr) {
    use can::Expr_::*;
    match &mut e.value {
        Negate(x) => simplify(x),
        Binop(_, _, _, l, r) => {
            simplify(l);
            simplify(r);
        }
        Lambda(_, body) => simplify(body),
        Call(func, args) => {
            simplify(func);
            for a in args {
                simplify(a);
            }
        }
        If(branches, otherwise) => {
            for (cond, then) in branches.iter_mut() {
                simplify(cond);
                simplify(then);
            }
            simplify(otherwise);
        }
        Let(decls, body) => {
            for d in decls.iter_mut() {
                match d {
                    can::LetDecl::Def(def) => simplify(&mut def.body),
                    can::LetDecl::Recursive(defs) => {
                        for def in defs {
                            simplify(&mut def.body);
                        }
                    }
                    can::LetDecl::Destruct(_, value) => simplify(value),
                }
            }
            simplify(body);
        }
        Case(scrut, branches) => {
            simplify(scrut);
            for (_, body) in branches.iter_mut() {
                simplify(body);
            }
        }
        List(items) => {
            for x in items {
                simplify(x);
            }
        }
        Access(record, _) => simplify(record),
        Update(record, fields) => {
            simplify(record);
            for (_, v) in fields {
                simplify(v);
            }
        }
        Record(fields) => {
            for (_, v) in fields {
                simplify(v);
            }
        }
        Tuple(a, b, rest) => {
            simplify(a);
            simplify(b);
            for x in rest {
                simplify(x);
            }
        }
        // Leaves and forms with nothing to fold.
        _ => {}
    }
    while let Some(replacement) = fold(e) {
        *e = replacement;
    }
}

/// One local fold of `e`, or `None` if nothing applies. Returns a whole
/// replacement node (see the module's type-preservation invariant).
fn fold(e: &can::Expr) -> Option<can::Expr> {
    use can::Expr_::*;
    match &e.value {
        Negate(x) => negate_lit(&x.value).map(|v| Located::new(e.region, v)),
        Binop(op, home, _, l, r) if home.as_str() == "Basics" => {
            if let Some(v) = fold_arith(op.as_str(), l, r) {
                return Some(Located::new(e.region, v));
            }
            // Short-circuit boolean identities. Only forms that preserve which
            // operands get evaluated are sound (Elm is pure, so the risk is
            // dropping a possibly-diverging operand): `True && x`, `False && x`,
            // `x && True`, and the `||` duals. NOT `x && False` / `x || True`,
            // which would drop `x`.
            match op.as_str() {
                "&&" => {
                    if as_bool(l) == Some(true) {
                        return Some((**r).clone());
                    }
                    if as_bool(l) == Some(false) {
                        return Some((**l).clone());
                    }
                    if as_bool(r) == Some(true) {
                        return Some((**l).clone());
                    }
                    None
                }
                "||" => {
                    if as_bool(l) == Some(false) {
                        return Some((**r).clone());
                    }
                    if as_bool(l) == Some(true) {
                        return Some((**l).clone());
                    }
                    if as_bool(r) == Some(false) {
                        return Some((**l).clone());
                    }
                    None
                }
                _ => None,
            }
        }
        If(branches, otherwise) => fold_if(branches, otherwise, e.region),
        _ => None,
    }
}

/// Collapse `if` branches with literal conditions. A `False` branch is dropped;
/// a `True` branch becomes the tail (later branches are dead). If no refutable
/// (non-literal) branch remains, the whole `if` collapses to its tail.
fn fold_if(
    branches: &[(can::Expr, can::Expr)],
    otherwise: &can::Expr,
    region: Region,
) -> Option<can::Expr> {
    let has_literal = branches.iter().any(|(c, _)| as_bool(c).is_some());
    if !has_literal {
        return None;
    }
    let mut kept: Vec<(can::Expr, can::Expr)> = Vec::new();
    let mut tail: Option<can::Expr> = None;
    for (cond, then) in branches {
        match as_bool(cond) {
            Some(true) => {
                tail = Some(then.clone());
                break;
            }
            Some(false) => {}
            None => kept.push((cond.clone(), then.clone())),
        }
    }
    let tail = tail.unwrap_or_else(|| otherwise.clone());
    if kept.is_empty() {
        // Collapses to the tail (its own region carries the right type — every
        // branch of an `if` shares the `if`'s type).
        Some(tail)
    } else {
        // A smaller `if` of the same type: reuse the original node's region.
        Some(Located::new(region, can::Expr_::If(kept, Box::new(tail))))
    }
}

/// `negate` of a numeric literal.
fn negate_lit(v: &can::Expr_) -> Option<can::Expr_> {
    match v {
        can::Expr_::Int(n) => safe_int(-(*n as i128)),
        can::Expr_::Float(f) => Some(can::Expr_::Float(-f)),
        _ => None,
    }
}

/// Fold a `Basics` binop on two literals to a literal, or `None`.
fn fold_arith(op: &str, l: &can::Expr, r: &can::Expr) -> Option<can::Expr_> {
    use can::Expr_::*;
    match (op, &l.value, &r.value) {
        ("+", Int(a), Int(b)) => safe_int(*a as i128 + *b as i128),
        ("-", Int(a), Int(b)) => safe_int(*a as i128 - *b as i128),
        ("*", Int(a), Int(b)) => safe_int(*a as i128 * *b as i128),
        // Elm: `n // 0 == 0`, and `//` truncates toward zero (like Rust `/`).
        ("//", Int(a), Int(b)) => {
            if !in_range(*a) || !in_range(*b) {
                None
            } else if *b == 0 {
                Some(Int(0))
            } else {
                safe_int(*a as i128 / *b as i128)
            }
        }
        ("+", Float(a), Float(b)) => finite_float(a + b),
        ("-", Float(a), Float(b)) => finite_float(a - b),
        ("*", Float(a), Float(b)) => finite_float(a * b),
        ("/", Float(a), Float(b)) if *b != 0.0 => finite_float(a / b),
        ("++", Str(a), Str(b)) => Some(Str(format!("{a}{b}"))),
        ("==", _, _) => lit_eq(l, r).map(bool_ctor),
        ("/=", _, _) => lit_eq(l, r).map(|b| bool_ctor(!b)),
        ("<", _, _) => lit_ord(l, r).map(|o| bool_ctor(o == Ordering::Less)),
        (">", _, _) => lit_ord(l, r).map(|o| bool_ctor(o == Ordering::Greater)),
        ("<=", _, _) => lit_ord(l, r).map(|o| bool_ctor(o != Ordering::Greater)),
        (">=", _, _) => lit_ord(l, r).map(|o| bool_ctor(o != Ordering::Less)),
        _ => None,
    }
}

fn in_range(n: i64) -> bool {
    (n as i128).abs() <= SAFE_INT
}

/// An `Int` literal, only if it (and its operands, checked by callers) stay in
/// the exactly-double-representable range so all backends agree.
fn safe_int(v: i128) -> Option<can::Expr_> {
    if v.abs() <= SAFE_INT {
        Some(can::Expr_::Int(v as i64))
    } else {
        None
    }
}

/// A `Float` literal, only if finite (don't bake in `Infinity`/`NaN`).
fn finite_float(v: f64) -> Option<can::Expr_> {
    if v.is_finite() {
        Some(can::Expr_::Float(v))
    } else {
        None
    }
}

/// Structural equality of two primitive literals of the same kind.
fn lit_eq(l: &can::Expr, r: &can::Expr) -> Option<bool> {
    use can::Expr_::*;
    match (&l.value, &r.value) {
        (Int(a), Int(b)) => Some(a == b),
        (Float(a), Float(b)) => Some(a == b),
        (Str(a), Str(b)) => Some(a == b),
        (Chr(a), Chr(b)) => Some(a == b),
        _ => None,
    }
}

/// Ordering of two numeric/char literals. `String` is excluded: Elm orders
/// strings by UTF-16 code units, which a byte-wise compare would not match.
fn lit_ord(l: &can::Expr, r: &can::Expr) -> Option<Ordering> {
    use can::Expr_::*;
    match (&l.value, &r.value) {
        (Int(a), Int(b)) => Some(a.cmp(b)),
        (Float(a), Float(b)) => a.partial_cmp(b),
        (Chr(a), Chr(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// The canonical `Basics.Bool` constructor for `b` (`True` = index 0).
fn bool_ctor(b: bool) -> can::Expr_ {
    let (name, index) = if b { ("True", 0) } else { ("False", 1) };
    can::Expr_::VarCtor(
        Name::from("Basics"),
        Name::from("Bool"),
        can::Ctor {
            name: Name::from(name),
            index,
            arity: 0,
            num_ctors: 2,
        },
    )
}

/// Whether `e` is the literal `Basics.Bool` `True` or `False`.
fn as_bool(e: &can::Expr) -> Option<bool> {
    if let can::Expr_::VarCtor(home, ty, ctor) = &e.value {
        if home.as_str() == "Basics" && ty.as_str() == "Bool" {
            return match ctor.name.as_str() {
                "True" => Some(true),
                "False" => Some(false),
                _ => None,
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use can::Expr_;

    fn e(v: Expr_) -> can::Expr {
        Located::new(Region::ZERO, v)
    }
    fn int(n: i64) -> can::Expr {
        e(Expr_::Int(n))
    }
    fn float(f: f64) -> can::Expr {
        e(Expr_::Float(f))
    }
    fn str_(s: &str) -> can::Expr {
        e(Expr_::Str(s.to_string()))
    }
    fn bin(op: &str, l: can::Expr, r: can::Expr) -> can::Expr {
        e(Expr_::Binop(
            Name::from(op),
            Name::from("Basics"),
            Name::from("fn"),
            Box::new(l),
            Box::new(r),
        ))
    }
    fn boolean(b: bool) -> can::Expr {
        e(bool_ctor(b))
    }
    fn local(n: &str) -> can::Expr {
        e(Expr_::VarLocal(Name::from(n)))
    }
    fn run(mut x: can::Expr) -> Expr_ {
        simplify(&mut x);
        x.value
    }

    #[test]
    fn folds_nested_int_arithmetic() {
        // 2 + 3 * 4  ->  14
        let x = bin("+", int(2), bin("*", int(3), int(4)));
        assert!(matches!(run(x), Expr_::Int(14)));
    }

    #[test]
    fn folds_float_and_string() {
        assert!(matches!(run(bin("/", float(3.0), float(2.0))), Expr_::Float(f) if f == 1.5));
        assert!(matches!(run(bin("++", str_("ab"), str_("c"))), Expr_::Str(s) if s == "abc"));
    }

    #[test]
    fn int_div_by_zero_is_zero() {
        assert!(matches!(run(bin("//", int(7), int(0))), Expr_::Int(0)));
    }

    #[test]
    fn comparisons_fold_to_bool() {
        assert_eq!(as_bool(&e(run(bin("<", int(1), int(2))))), Some(true));
        assert_eq!(as_bool(&e(run(bin("==", str_("a"), str_("b"))))), Some(false));
        assert_eq!(as_bool(&e(run(bin(">=", float(2.0), float(2.0))))), Some(true));
    }

    #[test]
    fn if_with_literal_condition_collapses() {
        // if True then 1 else 2  ->  1
        let x = e(Expr_::If(vec![(boolean(true), int(1))], Box::new(int(2))));
        assert!(matches!(run(x), Expr_::Int(1)));
        // if False then 1 else 2  ->  2
        let y = e(Expr_::If(vec![(boolean(false), int(1))], Box::new(int(2))));
        assert!(matches!(run(y), Expr_::Int(2)));
    }

    #[test]
    fn short_circuit_bool_identities() {
        // True && x -> x ; False || x -> x ; x && True -> x
        assert!(matches!(run(bin("&&", boolean(true), local("x"))), Expr_::VarLocal(n) if n.as_str() == "x"));
        assert!(matches!(run(bin("||", boolean(false), local("x"))), Expr_::VarLocal(n) if n.as_str() == "x"));
        assert!(matches!(run(bin("&&", local("x"), boolean(true))), Expr_::VarLocal(n) if n.as_str() == "x"));
        // False && x -> False (drops nothing Elm wouldn't; x is not evaluated)
        assert_eq!(as_bool(&e(run(bin("&&", boolean(false), local("x"))))), Some(false));
    }

    #[test]
    fn unsafe_bool_identities_are_left_alone() {
        // x && False must NOT fold (would drop a possibly-diverging x).
        assert!(matches!(run(bin("&&", local("x"), boolean(false))), Expr_::Binop(..)));
        // x || True likewise.
        assert!(matches!(run(bin("||", local("x"), boolean(true))), Expr_::Binop(..)));
    }

    #[test]
    fn does_not_fold_beyond_safe_integer_range() {
        // 2^53 + 2^53 would lose precision as a double; leave it for the runtime.
        let big = (1i64 << 53) as i64;
        assert!(matches!(run(bin("+", int(big), int(big))), Expr_::Binop(..)));
    }
}
