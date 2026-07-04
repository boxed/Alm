//! Dependency sorting of definitions (top-level and let) with cycle
//! legality checks: recursion is fine for functions and for values whose
//! self-references are delayed behind lambdas.

use super::*;

pub(super) fn sort_defs(defs: Vec<can::Def>) -> Result<Vec<can::DeclGroup>, Error> {
    let names: Vec<Name> = defs.iter().map(|d| d.name.value.clone()).collect();
    let name_set: HashSet<Name> = names.iter().cloned().collect();

    let to_deps = |refs: &HashSet<Name>| -> Vec<usize> {
        names
            .iter()
            .enumerate()
            .filter(|(_, n)| refs.contains(*n) && name_set.contains(*n))
            .map(|(i, _)| i)
            .collect()
    };
    let dependencies: Vec<Vec<usize>> = defs
        .iter()
        .map(|def| {
            let mut refs = HashSet::new();
            collect_refs(&def.body, &mut refs);
            to_deps(&refs)
        })
        .collect();
    // References evaluated immediately (not delayed inside a lambda);
    // only these make a value cycle illegal.
    let direct_dependencies: Vec<Vec<usize>> = defs
        .iter()
        .map(|def| {
            if !def.args.is_empty() {
                return vec![];
            }
            let mut refs = HashSet::new();
            collect_direct_refs(&def.body, &mut refs);
            to_deps(&refs)
        })
        .collect();

    // A value is illegal only if it sits on a cycle of IMMEDIATE
    // references; recursion delayed behind lambdas is fine.
    let directly_cyclic = cyclic_nodes(defs.len(), &direct_dependencies);
    for (i, def) in defs.iter().enumerate() {
        match directly_cyclic[i] {
            0 => {}
            1 => {
                return Err(Error::new(
                    format!(
                        "The value `{}` is defined in terms of itself, and it is not a function, so it would run forever.",
                        def.name.value
                    ),
                    def.name.region,
                ));
            }
            _ => {
                return Err(Error::new(
                    format!(
                        "The value `{}` is part of a definition cycle, and it is not a function, so it cannot be evaluated.",
                        def.name.value
                    ),
                    def.name.region,
                ));
            }
        }
    }

    let groups = graph::strongly_connected_components(defs.len(), &dependencies);

    let mut result = Vec::new();
    let mut defs: Vec<Option<can::Def>> = defs.into_iter().map(Some).collect();
    for group in groups {
        if group.len() == 1 {
            let i = group[0];
            let is_self_recursive = dependencies[i].contains(&i);
            let def = defs[i].take().unwrap();
            if is_self_recursive {
                result.push(can::DeclGroup::Recursive(vec![def]));
            } else {
                result.push(can::DeclGroup::Value(def));
            }
        } else {
            let group_defs: Vec<can::Def> =
                group.iter().map(|&i| defs[i].take().unwrap()).collect();
            result.push(can::DeclGroup::Recursive(group_defs));
        }
    }
    Ok(result)
}

/// For each node, the size of its cycle in the given graph: 0 when the
/// node is not on a cycle, 1 for a self-loop, otherwise the strongly
/// connected component size.
fn cyclic_nodes(count: usize, edges: &[Vec<usize>]) -> Vec<usize> {
    let components = graph::strongly_connected_components(count, edges);
    let mut cycle_size = vec![0; count];
    for component in components {
        if component.len() > 1 {
            for &i in &component {
                cycle_size[i] = component.len();
            }
        } else {
            let i = component[0];
            if edges[i].contains(&i) {
                cycle_size[i] = 1;
            }
        }
    }
    cycle_size
}

/// Collect names of local/top-level variables referenced in an expression.
/// With `direct_only`, stop at lambdas and function definitions: those
/// references only run when called, so they cannot make evaluation of a
/// value diverge (used for cycle legality).
fn collect_refs_help(expr: &can::Expr, direct_only: bool, refs: &mut HashSet<Name>) {
    use can::Expr_::*;
    let walk = |e: &can::Expr, refs: &mut HashSet<Name>| collect_refs_help(e, direct_only, refs);
    match &expr.value {
        VarLocal(name) | VarTopLevel(name) => {
            refs.insert(name.clone());
        }
        Binop(_, _, function, left, right) => {
            // A locally-defined operator's function is a real dependency.
            refs.insert(function.clone());
            walk(left, refs);
            walk(right, refs);
        }
        VarForeign(..) | VarCtor(..) | Chr(_) | Str(_) | Int(_) | Float(_) | Accessor(_)
        | Unit => {}
        Lambda(_, body) => {
            if !direct_only {
                walk(body, refs);
            }
        }
        List(items) => items.iter().for_each(|e| walk(e, refs)),
        Negate(e) => walk(e, refs),
        Call(f, args) => {
            walk(f, refs);
            args.iter().for_each(|e| walk(e, refs));
        }
        If(branches, otherwise) => {
            for (c, b) in branches {
                walk(c, refs);
                walk(b, refs);
            }
            walk(otherwise, refs);
        }
        Let(decls, body) => {
            for decl in decls {
                match decl {
                    can::LetDecl::Def(def) => {
                        if !direct_only || def.args.is_empty() {
                            walk(&def.body, refs);
                        }
                    }
                    can::LetDecl::Recursive(defs) => {
                        for def in defs {
                            if !direct_only || def.args.is_empty() {
                                walk(&def.body, refs);
                            }
                        }
                    }
                    can::LetDecl::Destruct(_, e) => walk(e, refs),
                }
            }
            walk(body, refs);
        }
        Case(scrutinee, branches) => {
            walk(scrutinee, refs);
            for (_, b) in branches {
                walk(b, refs);
            }
        }
        Access(e, _) => walk(e, refs),
        Update(e, fields) => {
            walk(e, refs);
            fields.iter().for_each(|(_, e)| walk(e, refs));
        }
        Record(fields) => fields.iter().for_each(|(_, e)| walk(e, refs)),
        Tuple(a, b, rest) => {
            walk(a, refs);
            walk(b, refs);
            rest.iter().for_each(|e| walk(e, refs));
        }
    }
}

fn collect_refs(expr: &can::Expr, refs: &mut HashSet<Name>) {
    collect_refs_help(expr, false, refs);
}

fn collect_direct_refs(expr: &can::Expr, refs: &mut HashSet<Name>) {
    collect_refs_help(expr, true, refs);
}

/// Sort let declarations into dependency order, grouping mutually recursive
/// functions. Cycles are only legal among function definitions.
pub(super) fn sort_let_decls(decls: Vec<can::LetDecl>) -> CResult<Vec<can::LetDecl>> {
    // Which names does each decl bind?
    let bound_names: Vec<Vec<Name>> = decls
        .iter()
        .map(|decl| match decl {
            can::LetDecl::Def(def) => vec![def.name.value.clone()],
            can::LetDecl::Recursive(defs) => {
                defs.iter().map(|d| d.name.value.clone()).collect()
            }
            can::LetDecl::Destruct(pattern, _) => pattern_names(pattern),
        })
        .collect();

    let name_to_decl: HashMap<Name, usize> = bound_names
        .iter()
        .enumerate()
        .flat_map(|(i, names)| names.iter().map(move |n| (n.clone(), i)))
        .collect();

    let dependencies: Vec<Vec<usize>> = decls
        .iter()
        .map(|decl| {
            let mut refs = HashSet::new();
            match decl {
                can::LetDecl::Def(def) => collect_refs(&def.body, &mut refs),
                can::LetDecl::Recursive(defs) => {
                    defs.iter().for_each(|d| collect_refs(&d.body, &mut refs))
                }
                can::LetDecl::Destruct(_, e) => collect_refs(e, &mut refs),
            }
            let mut deps: Vec<usize> = refs
                .iter()
                .filter_map(|name| name_to_decl.get(name).copied())
                .collect();
            deps.sort_unstable();
            deps.dedup();
            deps
        })
        .collect();

    // Immediate (non-delayed) references decide whether a cycle is legal.
    let direct_dependencies: Vec<Vec<usize>> = decls
        .iter()
        .map(|decl| {
            let mut refs = HashSet::new();
            match decl {
                can::LetDecl::Def(def) => {
                    if def.args.is_empty() {
                        collect_direct_refs(&def.body, &mut refs);
                    }
                }
                can::LetDecl::Recursive(defs) => {
                    for def in defs {
                        if def.args.is_empty() {
                            collect_direct_refs(&def.body, &mut refs);
                        }
                    }
                }
                can::LetDecl::Destruct(_, e) => collect_direct_refs(e, &mut refs),
            }
            let mut deps: Vec<usize> = refs
                .iter()
                .filter_map(|name| name_to_decl.get(name).copied())
                .collect();
            deps.sort_unstable();
            deps.dedup();
            deps
        })
        .collect();

    let directly_cyclic = cyclic_nodes(decls.len(), &direct_dependencies);
    for (i, decl) in decls.iter().enumerate() {
        if directly_cyclic[i] > 0 {
            let (message, region) = match decl {
                can::LetDecl::Def(def) => (
                    if directly_cyclic[i] == 1 {
                        format!(
                            "The value `{}` is defined in terms of itself, and it is not a function.",
                            def.name.value
                        )
                    } else {
                        format!(
                            "The value `{}` is part of a definition cycle, and it is not a function.",
                            def.name.value
                        )
                    },
                    def.name.region,
                ),
                can::LetDecl::Recursive(defs) => (
                    format!(
                        "The value `{}` is part of a definition cycle.",
                        defs[0].name.value
                    ),
                    defs[0].name.region,
                ),
                can::LetDecl::Destruct(pattern, _) => (
                    if directly_cyclic[i] == 1 {
                        "This destructuring refers to a name it binds.".to_string()
                    } else {
                        "This destructuring is part of a definition cycle.".to_string()
                    },
                    pattern.region,
                ),
            };
            return Err(Error::new(message, region));
        }
    }

    let groups = graph::strongly_connected_components(decls.len(), &dependencies);
    let mut decls: Vec<Option<can::LetDecl>> = decls.into_iter().map(Some).collect();
    let mut result = Vec::new();
    for group in groups {
        if group.len() == 1 {
            let i = group[0];
            let decl = decls[i].take().unwrap();
            if dependencies[i].contains(&i) {
                match decl {
                    can::LetDecl::Def(def) => {
                        result.push(can::LetDecl::Recursive(vec![def]));
                    }
                    can::LetDecl::Destruct(pattern, _) => {
                        return Err(Error::new(
                            "This destructuring refers to a name it binds.",
                            pattern.region,
                        ));
                    }
                    can::LetDecl::Recursive(_) => unreachable!(),
                }
            } else {
                result.push(decl);
            }
        } else {
            let mut group_defs = Vec::new();
            for &i in &group {
                match decls[i].take().unwrap() {
                    can::LetDecl::Def(def) => group_defs.push(def),
                    can::LetDecl::Recursive(defs) => group_defs.extend(defs),
                    can::LetDecl::Destruct(pattern, _) => {
                        return Err(Error::new(
                            "This destructuring is part of a definition cycle.",
                            pattern.region,
                        ));
                    }
                }
            }
            result.push(can::LetDecl::Recursive(group_defs));
        }
    }
    Ok(result)
}

pub(super) fn pattern_names(pattern: &can::Pattern) -> Vec<Name> {
    let mut names = Vec::new();
    pattern_names_help(pattern, &mut names);
    names
}

fn pattern_names_help(pattern: &can::Pattern, names: &mut Vec<Name>) {
    use can::Pattern_::*;
    match &pattern.value {
        Anything | Unit | Chr(_) | Str(_) | Int(_) => {}
        Var(name) => names.push(name.clone()),
        Record(fields) => names.extend(fields.iter().map(|f| f.value.clone())),
        Alias(inner, name) => {
            pattern_names_help(inner, names);
            names.push(name.value.clone());
        }
        Tuple(a, b, rest) => {
            pattern_names_help(a, names);
            pattern_names_help(b, names);
            rest.iter().for_each(|p| pattern_names_help(p, names));
        }
        Ctor(_, _, _, args) => args.iter().for_each(|p| pattern_names_help(p, names)),
        List(items) => items.iter().for_each(|p| pattern_names_help(p, names)),
        Cons(head, tail) => {
            pattern_names_help(head, names);
            pattern_names_help(tail, names);
        }
    }
}
