//! Function inlining on the monomorphized IR (post-`mono`), consumed by the
//! WasmGC backend. Every `TypedExpr` node already carries its concrete `.tipe`
//! here, so splicing a callee's body into a call site needs no re-typing and has
//! none of the `Region`-based hazards of a canonical-AST rewrite.
//!
//! A saturated call to a *small, non-recursive* top-level function whose
//! parameters are simple (`Var`/`_`) is replaced by `let <params> = <args> in
//! <body>`. Top-level monomorphized bodies are closed (their only free locals
//! are their params), and args are evaluated in the caller's scope; the one
//! capture risk — a later arg mentioning a caller variable whose name collides
//! with a param — is removed by alpha-renaming params to fresh `$`-prefixed
//! names (which cannot occur in source Elm), via a scope-aware substitution.
//!
//! Recursion (direct or mutual) is prevented from expanding forever by an
//! "active" set along the inline stack, plus a per-function inline budget.
//!
//! **OFF BY DEFAULT — opt in with `ALM_INLINE=1`.** Benchmarking showed inlining
//! is net-negative on the WasmGC backend: its direct `call`s are already cheap,
//! and the `let`-bindings this pass introduces to bind params defeat the
//! backend's scalar-unboxing ABI (and add redundant local copies), so hot scalar
//! arithmetic that would run unboxed through a specialized function instead
//! boxes/copies through the inlined lets — ~2x slower on a call-dominated loop,
//! neutral otherwise. JS and native get inlining from V8 / LLVM regardless. The
//! pass is kept (correct, capture-safe) for the day the scalar-unboxing handles
//! synthesized lets, but it does nothing unless explicitly enabled.

use std::collections::{HashMap, HashSet};

use crate::ast::canonical as can;
use crate::data::Name;

use super::mono::{MonoProgram, TypedExpr, TypedFn, TypedKind, TypedLetDecl};

/// Largest callee body (node count) considered "small" enough to inline.
const MAX_BODY_SIZE: usize = 12;
/// Cap on inlines performed while rewriting a single function, to bound growth.
const MAX_INLINES_PER_FN: usize = 400;

struct Candidate {
    params: Vec<(can::Pattern, can::Type)>,
    body: TypedExpr,
}

/// Inline calls to small non-recursive functions throughout `program`.
pub fn inline(program: &mut MonoProgram) {
    // Opt-in only: inlining regresses the WasmGC backend (see the module doc).
    if std::env::var("ALM_INLINE").is_err() {
        return;
    }
    let mut cands: HashMap<Name, Candidate> = HashMap::new();
    for f in &program.functions {
        if is_candidate(f) {
            cands.insert(
                f.mangled.clone(),
                Candidate {
                    params: f.params.clone(),
                    body: f.body.clone(),
                },
            );
        }
    }
    if cands.is_empty() {
        return;
    }
    let mut counter = 0usize;
    for f in &mut program.functions {
        let mut active = HashSet::new();
        active.insert(f.mangled.clone()); // never inline a call back into f itself
        let mut budget = MAX_INLINES_PER_FN;
        inline_expr(&mut f.body, &cands, &mut active, &mut counter, &mut budget);
    }
}

/// Small, non-self-recursive, simple-parameter functions are inline candidates.
fn is_candidate(f: &TypedFn) -> bool {
    f.params.iter().all(|(p, _)| is_simple_param(p))
        && size(&f.body, MAX_BODY_SIZE + 1) <= MAX_BODY_SIZE
        && !calls_global(&f.body, &f.mangled)
}

fn is_simple_param(p: &can::Pattern) -> bool {
    matches!(p.value, can::Pattern_::Var(_) | can::Pattern_::Anything)
}

fn inline_expr(
    e: &mut TypedExpr,
    cands: &HashMap<Name, Candidate>,
    active: &mut HashSet<Name>,
    counter: &mut usize,
    budget: &mut usize,
) {
    // Rewrite sub-expressions first (so args are inlined before we splice).
    for child in children_mut(e) {
        inline_expr(child, cands, active, counter, budget);
    }
    if *budget == 0 {
        return;
    }
    // A saturated direct call to an inline candidate not already on the stack.
    let target = match &e.kind {
        TypedKind::Call(callee, args) => match &callee.kind {
            TypedKind::Global(m)
                if !active.contains(m)
                    && cands.get(m).is_some_and(|c| c.params.len() == args.len()) =>
            {
                Some(m.clone())
            }
            _ => None,
        },
        _ => None,
    };
    let Some(m) = target else { return };
    let args = match &e.kind {
        TypedKind::Call(_, args) => args.clone(),
        _ => unreachable!(),
    };
    *budget -= 1;
    let cand = &cands[&m];
    *e = build_inline(cand, &args, e.tipe.clone(), e.region, counter);
    // Inline nested calls inside the spliced body, guarding against re-entering m.
    if let TypedKind::Let(_, body) = &mut e.kind {
        active.insert(m.clone());
        inline_expr(body, cands, active, counter, budget);
        active.remove(&m);
    }
}

/// Build `let <fresh params> = <args> in <renamed body>` for one inlined call.
fn build_inline(
    cand: &Candidate,
    args: &[TypedExpr],
    tipe: can::Type,
    region: crate::reporting::Region,
    counter: &mut usize,
) -> TypedExpr {
    let mut body = cand.body.clone();
    let mut rename: HashMap<Name, Name> = HashMap::new();
    let mut decls: Vec<TypedLetDecl> = Vec::with_capacity(args.len());
    for ((pat, _), arg) in cand.params.iter().zip(args) {
        let orig = match &pat.value {
            can::Pattern_::Var(n) => n.clone(),
            _ => Name::from("_"), // Anything: bind a throwaway to keep arg's evaluation
        };
        let fresh = fresh_name(&orig, counter);
        if let can::Pattern_::Var(n) = &pat.value {
            rename.insert(n.clone(), fresh.clone());
        }
        decls.push(TypedLetDecl::Def {
            name: fresh,
            params: Vec::new(),
            body: arg.clone(),
        });
    }
    // Params renamed to fresh names, so args (which reference caller variables)
    // cannot be captured by the let bindings; a single group is safe.
    if !rename.is_empty() {
        subst(&mut body, &rename);
    }
    TypedExpr {
        tipe,
        region,
        kind: TypedKind::Let(decls, Box::new(body)),
    }
}

fn fresh_name(orig: &Name, counter: &mut usize) -> Name {
    let n = *counter;
    *counter += 1;
    // Leading `$` cannot appear in a source Elm identifier, so this never
    // collides with a user name or another fresh name.
    Name::from(format!("${orig}$i{n}"))
}

// --- scope-aware substitution of renamed parameter names --------------------

fn subst(e: &mut TypedExpr, map: &HashMap<Name, Name>) {
    if map.is_empty() {
        return;
    }
    match &mut e.kind {
        TypedKind::Local(n) => {
            if let Some(f) = map.get(n) {
                *n = f.clone();
            }
        }
        TypedKind::Lambda(params, body) => {
            let bound = params_names(params);
            subst(body, &without(map, &bound));
        }
        TypedKind::Case(scrut, branches) => {
            subst(scrut, map);
            for (pat, body) in branches {
                subst(body, &without(map, &pat_names(pat)));
            }
        }
        TypedKind::Let(decls, body) => {
            let bound = let_bound_names(decls);
            let inner = without(map, &bound);
            for d in decls.iter_mut() {
                subst_decl(d, &inner);
            }
            subst(body, &inner);
        }
        // Structural recursion, no new bindings.
        _ => {
            for child in children_mut(e) {
                subst(child, map);
            }
        }
    }
}

fn subst_decl(d: &mut TypedLetDecl, map: &HashMap<Name, Name>) {
    match d {
        TypedLetDecl::Def { params, body, .. } => {
            subst(body, &without(map, &params_names(params)));
        }
        TypedLetDecl::Destruct(_, value) => subst(value, map),
        TypedLetDecl::Recursive(ds) => {
            for d in ds {
                subst_decl(d, map);
            }
        }
    }
}

fn without(map: &HashMap<Name, Name>, bound: &[Name]) -> HashMap<Name, Name> {
    if bound.iter().all(|b| !map.contains_key(b)) {
        return map.clone();
    }
    map.iter()
        .filter(|(k, _)| !bound.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

// --- small helpers over the mono IR -----------------------------------------

/// Mutable references to every immediate sub-expression of `e`.
fn children_mut(e: &mut TypedExpr) -> Vec<&mut TypedExpr> {
    let mut out: Vec<&mut TypedExpr> = Vec::new();
    match &mut e.kind {
        TypedKind::Negate(x) | TypedKind::Access(x, _) => out.push(x),
        TypedKind::Binop(_, _, _, l, r) => {
            out.push(l);
            out.push(r);
        }
        TypedKind::Call(f, args) => {
            out.push(f);
            out.extend(args.iter_mut());
        }
        TypedKind::If(branches, otherwise) => {
            for (c, t) in branches {
                out.push(c);
                out.push(t);
            }
            out.push(otherwise);
        }
        TypedKind::Let(decls, body) => {
            for d in decls {
                decl_children_mut(d, &mut out);
            }
            out.push(body);
        }
        TypedKind::Case(scrut, branches) => {
            out.push(scrut);
            for (_, b) in branches {
                out.push(b);
            }
        }
        TypedKind::Lambda(_, body) => out.push(body),
        TypedKind::List(xs) => out.extend(xs.iter_mut()),
        TypedKind::Record(fs) => out.extend(fs.iter_mut().map(|(_, v)| v)),
        TypedKind::Update(r, fs) => {
            out.push(r);
            out.extend(fs.iter_mut().map(|(_, v)| v));
        }
        TypedKind::Tuple(a, b, c) => {
            out.push(a);
            out.push(b);
            if let Some(c) = c {
                out.push(c);
            }
        }
        _ => {}
    }
    out
}

fn decl_children_mut<'a>(d: &'a mut TypedLetDecl, out: &mut Vec<&'a mut TypedExpr>) {
    match d {
        TypedLetDecl::Def { body, .. } => out.push(body),
        TypedLetDecl::Destruct(_, value) => out.push(value),
        TypedLetDecl::Recursive(ds) => {
            for d in ds {
                decl_children_mut(d, out);
            }
        }
    }
}

/// Node count of `e`, stopping once it exceeds `cap` (returns `cap + 1`).
fn size(e: &TypedExpr, cap: usize) -> usize {
    fn go(e: &TypedExpr, cap: usize, n: &mut usize) {
        if *n > cap {
            return;
        }
        *n += 1;
        // Re-borrow immutably by walking through a temporary clone-free path:
        // reuse children_mut via a raw pass is not possible on &, so recurse
        // by matching the same shapes.
        match &e.kind {
            TypedKind::Negate(x) | TypedKind::Access(x, _) => go(x, cap, n),
            TypedKind::Binop(_, _, _, l, r) => {
                go(l, cap, n);
                go(r, cap, n);
            }
            TypedKind::Call(f, args) => {
                go(f, cap, n);
                args.iter().for_each(|a| go(a, cap, n));
            }
            TypedKind::If(branches, otherwise) => {
                for (c, t) in branches {
                    go(c, cap, n);
                    go(t, cap, n);
                }
                go(otherwise, cap, n);
            }
            TypedKind::Let(decls, body) => {
                for d in decls {
                    decl_size(d, cap, n);
                }
                go(body, cap, n);
            }
            TypedKind::Case(scrut, branches) => {
                go(scrut, cap, n);
                for (_, b) in branches {
                    go(b, cap, n);
                }
            }
            TypedKind::Lambda(_, body) => go(body, cap, n),
            TypedKind::List(xs) => xs.iter().for_each(|x| go(x, cap, n)),
            TypedKind::Record(fs) => fs.iter().for_each(|(_, v)| go(v, cap, n)),
            TypedKind::Update(r, fs) => {
                go(r, cap, n);
                fs.iter().for_each(|(_, v)| go(v, cap, n));
            }
            TypedKind::Tuple(a, b, c) => {
                go(a, cap, n);
                go(b, cap, n);
                if let Some(c) = c {
                    go(c, cap, n);
                }
            }
            _ => {}
        }
    }
    fn decl_size(d: &TypedLetDecl, cap: usize, n: &mut usize) {
        match d {
            TypedLetDecl::Def { body, .. } => go(body, cap, n),
            TypedLetDecl::Destruct(_, value) => go(value, cap, n),
            TypedLetDecl::Recursive(ds) => ds.iter().for_each(|d| decl_size(d, cap, n)),
        }
    }
    let mut n = 0;
    go(e, cap, &mut n);
    n
}

/// Does `e` reference the global `name` (i.e. call itself, directly)?
fn calls_global(e: &TypedExpr, name: &Name) -> bool {
    if let TypedKind::Global(g) = &e.kind {
        if g == name {
            return true;
        }
    }
    // `children_mut` needs `&mut`; walk immutably here.
    fn any(e: &TypedExpr, name: &Name) -> bool {
        if let TypedKind::Global(g) = &e.kind {
            if g == name {
                return true;
            }
        }
        match &e.kind {
            TypedKind::Negate(x) | TypedKind::Access(x, _) => any(x, name),
            TypedKind::Binop(_, _, _, l, r) => any(l, name) || any(r, name),
            TypedKind::Call(f, args) => any(f, name) || args.iter().any(|a| any(a, name)),
            TypedKind::If(branches, otherwise) => {
                branches.iter().any(|(c, t)| any(c, name) || any(t, name)) || any(otherwise, name)
            }
            TypedKind::Let(decls, body) => {
                decls.iter().any(|d| decl_any(d, name)) || any(body, name)
            }
            TypedKind::Case(scrut, branches) => {
                any(scrut, name) || branches.iter().any(|(_, b)| any(b, name))
            }
            TypedKind::Lambda(_, body) => any(body, name),
            TypedKind::List(xs) => xs.iter().any(|x| any(x, name)),
            TypedKind::Record(fs) => fs.iter().any(|(_, v)| any(v, name)),
            TypedKind::Update(r, fs) => any(r, name) || fs.iter().any(|(_, v)| any(v, name)),
            TypedKind::Tuple(a, b, c) => {
                any(a, name) || any(b, name) || c.as_ref().is_some_and(|c| any(c, name))
            }
            _ => false,
        }
    }
    fn decl_any(d: &TypedLetDecl, name: &Name) -> bool {
        match d {
            TypedLetDecl::Def { body, .. } => any(body, name),
            TypedLetDecl::Destruct(_, value) => any(value, name),
            TypedLetDecl::Recursive(ds) => ds.iter().any(|d| decl_any(d, name)),
        }
    }
    any(e, name)
}

fn params_names(params: &[(can::Pattern, can::Type)]) -> Vec<Name> {
    params.iter().flat_map(|(p, _)| pat_names(p)).collect()
}

fn let_bound_names(decls: &[TypedLetDecl]) -> Vec<Name> {
    let mut out = Vec::new();
    for d in decls {
        decl_bound(d, &mut out);
    }
    out
}

fn decl_bound(d: &TypedLetDecl, out: &mut Vec<Name>) {
    match d {
        TypedLetDecl::Def { name, .. } => out.push(name.clone()),
        TypedLetDecl::Destruct(pat, _) => out.extend(pat_names(pat)),
        TypedLetDecl::Recursive(ds) => {
            for d in ds {
                decl_bound(d, out);
            }
        }
    }
}

/// Variable names a pattern binds.
fn pat_names(p: &can::Pattern) -> Vec<Name> {
    use can::Pattern_::*;
    match &p.value {
        Var(n) => vec![n.clone()],
        Alias(inner, n) => {
            let mut v = pat_names(inner);
            v.push(n.value.clone());
            v
        }
        Ctor(_, _, _, args) => args.iter().flat_map(pat_names).collect(),
        Tuple(a, b, rest) => {
            let mut v = pat_names(a);
            v.extend(pat_names(b));
            v.extend(rest.iter().flat_map(pat_names));
            v
        }
        List(items) => items.iter().flat_map(pat_names).collect(),
        Cons(h, t) => {
            let mut v = pat_names(h);
            v.extend(pat_names(t));
            v
        }
        Record(fields) => fields.iter().map(|f| f.value.clone()).collect(),
        Anything | Unit | Int(_) | Str(_) | Chr(_) => Vec::new(),
    }
}
