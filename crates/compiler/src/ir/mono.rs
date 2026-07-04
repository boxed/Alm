//! Monomorphization — phase 2.
//!
//! Starting from `main` (whose type is fully concrete), walk the call graph
//! and stamp out one specialization of every polymorphic function per
//! concrete type it is used at. This is the analysis half: it discovers the
//! set of `(function, concrete type)` instances that a typed backend must
//! emit, plus the concrete signatures at which built-in/kernel functions and
//! constructors are used (so later phases can generate typed kernels).
//!
//! It relies on the per-expression types captured by the checker
//! ([`crate::typecheck::Checked::node_types`]): at a reference to another
//! function `g`, the captured type of that reference node — expressed in the
//! *current* function's scheme variables — is substituted through the current
//! specialization to yield `g`'s concrete type here.
//!
//! What it does *not* yet do: build the specialized function bodies, cross
//! module boundaries, or choose representations. Those are later phases; this
//! pass pins down the specialization set and is exercised directly by tests.

use std::collections::HashMap;

use crate::ast::canonical as can;
use crate::data::Name;
use crate::reporting::Region;

/// One concrete instance of a top-level function: its name and the concrete
/// type it is specialized at. Two uses of a polymorphic function at the same
/// concrete type share an instance.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub name: Name,
    pub tipe: can::Type,
}

/// A use of a built-in/kernel function (`VarForeign`) at a concrete type.
/// Later phases generate a typed kernel per distinct signature so unboxed
/// values need not be re-tagged at the boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignUse {
    pub module: Name,
    pub name: Name,
    pub tipe: can::Type,
}

/// The result of the analysis: every reachable function specialization and
/// every concrete built-in/constructor use.
#[derive(Debug, Default)]
pub struct MonoSet {
    pub instances: Vec<Instance>,
    pub foreign_uses: Vec<ForeignUse>,
}

/// Compute the specializations reachable from `main` within a single module.
///
/// * `types` — every top-level definition's generalized type.
/// * `node_types` — every expression's concrete type, keyed by region, with
///   variable names aligned to the enclosing definition's scheme.
pub fn analyze(
    module: &can::Module,
    types: &HashMap<Name, can::Type>,
    node_types: &HashMap<Region, can::Type>,
) -> MonoSet {
    let defs = index_defs(module);
    let mut set = MonoSet::default();
    // Instances already discovered, keyed by (name, printed type), so a
    // recursive or repeatedly-used function specializes once per type.
    let mut seen: HashMap<(Name, String), ()> = HashMap::new();
    let mut queue: Vec<Instance> = Vec::new();

    if let Some(main_ty) = types.get(&Name::from("main")) {
        enqueue(
            &mut queue,
            &mut seen,
            &mut set,
            Instance {
                name: Name::from("main"),
                tipe: main_ty.clone(),
            },
        );
    }

    while let Some(instance) = queue.pop() {
        let Some(def) = defs.get(&instance.name) else {
            continue; // referenced but not defined here (e.g. a port)
        };
        // Match the function's generic scheme against this concrete use to
        // get the substitution for its type variables.
        let scheme = types
            .get(&instance.name)
            .cloned()
            .unwrap_or_else(|| instance.tipe.clone());
        let mut subst = HashMap::new();
        match_type(&scheme, &instance.tipe, &mut subst);

        // Walk the body; every reference node's captured type, put through
        // the substitution, is the concrete type of the referent here.
        let mut refs = Vec::new();
        collect_refs(&def.body, &mut refs);
        for node in refs {
            let Some(captured) = node_types.get(&node.region) else {
                continue;
            };
            let concrete = apply_subst(&subst, captured);
            match &node.value {
                can::Expr_::VarTopLevel(name) if defs.contains_key(name) => {
                    enqueue(
                        &mut queue,
                        &mut seen,
                        &mut set,
                        Instance {
                            name: name.clone(),
                            tipe: concrete,
                        },
                    );
                }
                can::Expr_::VarForeign(module, name) => {
                    record_foreign(
                        &mut set,
                        ForeignUse {
                            module: module.clone(),
                            name: name.clone(),
                            tipe: concrete,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    set
}

fn enqueue(
    queue: &mut Vec<Instance>,
    seen: &mut HashMap<(Name, String), ()>,
    set: &mut MonoSet,
    instance: Instance,
) {
    let key = (instance.name.clone(), format!("{:?}", instance.tipe));
    if seen.insert(key, ()).is_some() {
        return;
    }
    set.instances.push(instance.clone());
    queue.push(instance);
}

fn record_foreign(set: &mut MonoSet, use_: ForeignUse) {
    if !set.foreign_uses.contains(&use_) {
        set.foreign_uses.push(use_);
    }
}

/// Build a name -> definition index over top-level declarations.
fn index_defs(module: &can::Module) -> HashMap<Name, &can::Def> {
    let mut defs = HashMap::new();
    for group in &module.decls {
        match group {
            can::DeclGroup::Value(def) => {
                defs.insert(def.name.value.clone(), def);
            }
            can::DeclGroup::Recursive(group) => {
                for def in group {
                    defs.insert(def.name.value.clone(), def);
                }
            }
        }
    }
    defs
}

/// Collect every reference-like node (a var, foreign, or constructor) in an
/// expression tree. These are the call-graph edges.
fn collect_refs<'a>(expr: &'a can::Expr, out: &mut Vec<&'a can::Expr>) {
    use can::Expr_::*;
    match &expr.value {
        VarLocal(_) | VarTopLevel(_) | VarForeign(..) | VarCtor(..) => out.push(expr),
        Negate(inner) => collect_refs(inner, out),
        List(items) => items.iter().for_each(|e| collect_refs(e, out)),
        Binop(_, _, _, l, r) => {
            collect_refs(l, out);
            collect_refs(r, out)
        }
        Lambda(_, body) => collect_refs(body, out),
        Call(f, args) => {
            collect_refs(f, out);
            args.iter().for_each(|e| collect_refs(e, out))
        }
        If(branches, otherwise) => {
            for (c, b) in branches {
                collect_refs(c, out);
                collect_refs(b, out)
            }
            collect_refs(otherwise, out)
        }
        Let(decls, body) => {
            for decl in decls {
                match decl {
                    can::LetDecl::Def(def) => collect_refs(&def.body, out),
                    can::LetDecl::Recursive(defs) => {
                        defs.iter().for_each(|d| collect_refs(&d.body, out))
                    }
                    can::LetDecl::Destruct(_, value) => collect_refs(value, out),
                }
            }
            collect_refs(body, out)
        }
        Case(scrutinee, branches) => {
            collect_refs(scrutinee, out);
            branches.iter().for_each(|(_, b)| collect_refs(b, out))
        }
        Access(record, _) => collect_refs(record, out),
        Update(record, fields) => {
            collect_refs(record, out);
            fields.iter().for_each(|(_, v)| collect_refs(v, out))
        }
        Record(fields) => fields.iter().for_each(|(_, v)| collect_refs(v, out)),
        Tuple(a, b, rest) => {
            collect_refs(a, out);
            collect_refs(b, out);
            rest.iter().for_each(|e| collect_refs(e, out))
        }
        Chr(_) | Str(_) | Int(_) | Float(_) | Accessor(_) | Unit => {}
    }
}

/// One-directional match: `generic` carries the type variables, `concrete`
/// is (ideally) ground. Bind each variable to the concrete type facing it.
/// Structural mismatches are tolerated — an unbound variable simply stays
/// unbound, which is fine for phantom variables.
fn match_type(generic: &can::Type, concrete: &can::Type, subst: &mut HashMap<Name, can::Type>) {
    use can::Type::*;
    match (generic, concrete) {
        (Var(name), _) => {
            subst.entry(name.clone()).or_insert_with(|| concrete.clone());
        }
        (Lambda(a1, b1), Lambda(a2, b2)) => {
            match_type(a1, a2, subst);
            match_type(b1, b2, subst);
        }
        (Type(_, _, args1), Type(_, _, args2)) if args1.len() == args2.len() => {
            for (a, b) in args1.iter().zip(args2) {
                match_type(a, b, subst);
            }
        }
        (Tuple(a1, b1, c1), Tuple(a2, b2, c2)) => {
            match_type(a1, a2, subst);
            match_type(b1, b2, subst);
            if let (Some(c1), Some(c2)) = (c1, c2) {
                match_type(c1, c2, subst);
            }
        }
        (Record(f1, _), Record(f2, _)) => {
            for (name, t1) in f1 {
                if let Some((_, t2)) = f2.iter().find(|(n, _)| n == name) {
                    match_type(t1, t2, subst);
                }
            }
        }
        _ => {}
    }
}

/// Apply a substitution to a type, replacing bound variables.
fn apply_subst(subst: &HashMap<Name, can::Type>, tipe: &can::Type) -> can::Type {
    use can::Type::*;
    match tipe {
        Var(name) => subst.get(name).cloned().unwrap_or_else(|| tipe.clone()),
        Lambda(a, b) => Lambda(
            Box::new(apply_subst(subst, a)),
            Box::new(apply_subst(subst, b)),
        ),
        Type(home, name, args) => Type(
            home.clone(),
            name.clone(),
            args.iter().map(|a| apply_subst(subst, a)).collect(),
        ),
        Record(fields, ext) => Record(
            fields
                .iter()
                .map(|(n, t)| (n.clone(), apply_subst(subst, t)))
                .collect(),
            ext.clone(),
        ),
        Tuple(a, b, c) => Tuple(
            Box::new(apply_subst(subst, a)),
            Box::new(apply_subst(subst, b)),
            c.as_ref().map(|c| Box::new(apply_subst(subst, c))),
        ),
        Unit => Unit,
    }
}
