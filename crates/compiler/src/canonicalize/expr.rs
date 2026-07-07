//! Expression, pattern, and definition canonicalization: name
//! resolution against the environment.

use super::*;
use crate::reporting::annotation::Position;

pub(super) fn canonicalize_value(env: &mut Env, value: &src::Value) -> CResult<can::Def> {
    let annotation = match &value.type_annotation {
        Some(tipe) => Some(canonicalize_type(env, tipe)?),
        None => None,
    };
    let mut bound = Vec::new();
    let args = value
        .args
        .iter()
        .map(|p| canonicalize_pattern(env, p, &mut bound))
        .collect::<CResult<Vec<_>>>()?;
    env.scopes.push(bound.into_iter().collect());
    let body = canonicalize_expr(env, &value.body);
    env.scopes.pop();
    Ok(can::Def {
        name: value.name.clone(),
        args,
        body: body?,
        annotation,
    })
}

fn canonicalize_pattern(
    env: &Env,
    pattern: &src::Pattern,
    bound: &mut Vec<Name>,
) -> CResult<can::Pattern> {
    let region = pattern.region;
    let bind = |name: &Located<Name>, bound: &mut Vec<Name>| -> CResult<()> {
        if bound.contains(&name.value) {
            Err(Error::new(
                format!("The name `{}` is bound more than once in this pattern.", name.value),
                name.region,
            ))
        } else {
            bound.push(name.value.clone());
            Ok(())
        }
    };

    let pattern_ = match &pattern.value {
        src::Pattern_::Anything => can::Pattern_::Anything,
        src::Pattern_::Var(name) => {
            bind(&Located::new(region, name.clone()), bound)?;
            can::Pattern_::Var(name.clone())
        }
        src::Pattern_::Record(fields) => {
            for field in fields {
                bind(field, bound)?;
            }
            can::Pattern_::Record(fields.clone())
        }
        src::Pattern_::Alias(inner, name) => {
            let inner = canonicalize_pattern(env, inner, bound)?;
            bind(name, bound)?;
            can::Pattern_::Alias(Box::new(inner), name.clone())
        }
        src::Pattern_::Unit => can::Pattern_::Unit,
        src::Pattern_::Tuple(a, b, rest) => {
            if rest.len() > 1 {
                return Err(Error::new("Tuples can only hold two or three values.", region));
            }
            can::Pattern_::Tuple(
                Box::new(canonicalize_pattern(env, a, bound)?),
                Box::new(canonicalize_pattern(env, b, bound)?),
                rest.iter()
                    .map(|p| canonicalize_pattern(env, p, bound))
                    .collect::<CResult<Vec<_>>>()?,
            )
        }
        src::Pattern_::Ctor(name_region, name, args) => {
            let info = find_ctor(env, *name_region, None, name)?;
            canonicalize_ctor_pattern(env, *name_region, info, args, bound)?
        }
        src::Pattern_::CtorQual(name_region, qualifier, name, args) => {
            let info = find_ctor(env, *name_region, Some(qualifier), name)?;
            canonicalize_ctor_pattern(env, *name_region, info, args, bound)?
        }
        src::Pattern_::List(items) => can::Pattern_::List(
            items
                .iter()
                .map(|p| canonicalize_pattern(env, p, bound))
                .collect::<CResult<Vec<_>>>()?,
        ),
        src::Pattern_::Cons(head, tail) => can::Pattern_::Cons(
            Box::new(canonicalize_pattern(env, head, bound)?),
            Box::new(canonicalize_pattern(env, tail, bound)?),
        ),
        src::Pattern_::Chr(c) => can::Pattern_::Chr(*c),
        src::Pattern_::Str(s) => can::Pattern_::Str(s.clone()),
        src::Pattern_::Int(n) => can::Pattern_::Int(*n),
    };
    Ok(Located::new(region, pattern_))
}

fn canonicalize_ctor_pattern(
    env: &Env,
    region: Region,
    info: CtorInfo,
    args: &[src::Pattern],
    bound: &mut Vec<Name>,
) -> CResult<can::Pattern_> {
    if args.len() as u32 != info.ctor.arity {
        return Err(Error::new(
            format!(
                "The `{}` constructor needs {} argument{}, but I see {}.",
                info.ctor.name,
                info.ctor.arity,
                if info.ctor.arity == 1 { "" } else { "s" },
                args.len()
            ),
            region,
        ));
    }
    Ok(can::Pattern_::Ctor(
        info.home,
        info.union,
        info.ctor,
        args.iter()
            .map(|p| canonicalize_pattern(env, p, bound))
            .collect::<CResult<Vec<_>>>()?,
    ))
}

fn find_ctor(
    env: &Env,
    region: Region,
    qualifier: Option<&Name>,
    name: &Name,
) -> CResult<CtorInfo> {
    match qualifier {
        None => {
            if let Some(info) = env.ctors.get(name) {
                return Ok(info.clone());
            }
            if let Some((union, index)) = builtins::lookup_exposed_ctor(name.as_str()) {
                return Ok(builtin_ctor_info(union, index));
            }
            Err(Error::new(
                format!("I cannot find a `{}` constructor.", name),
                region,
            ))
        }
        Some(qualifier) => {
            let candidates = env.resolve_modules(qualifier);
            if candidates.is_empty() {
                return Err(Error::new(
                    format!("I cannot find a module named `{}`.", qualifier),
                    region,
                ));
            }
            for module in &candidates {
                if *module == env.module_name {
                    if let Some(info) = env.ctors.get(name) {
                        return Ok(info.clone());
                    }
                }
                if let Some((union, index)) =
                    builtins::lookup_ctor(module.as_str(), name.as_str())
                {
                    return Ok(builtin_ctor_info(union, index));
                }
                if let Some(interface) = env.interfaces.get(module) {
                    for union_name in &interface.open_unions {
                        let union = &interface.unions[union_name];
                        if let Some(ctor) = union.ctors.iter().find(|c| c.name == *name) {
                            return Ok(CtorInfo {
                                home: module.clone(),
                                union: union.name.clone(),
                                ctor: can::Ctor {
                                    name: ctor.name.clone(),
                                    index: ctor.index,
                                    arity: ctor.args.len() as u32,
                                    num_ctors: union.ctors.len() as u32,
                                },
                            });
                        }
                    }
                }
            }
            Err(Error::new(
                format!(
                    "The `{}` module does not have a `{}` constructor.",
                    candidates[0], name
                ),
                region,
            ))
        }
    }
}

pub(super) fn builtin_ctor_info(union: &'static builtins::BuiltinUnion, index: u32) -> CtorInfo {
    let (ctor_name, args) = union.ctors[index as usize];
    CtorInfo {
        home: Name::from(union.module),
        union: Name::from(union.name),
        ctor: can::Ctor {
            name: Name::from(ctor_name),
            index,
            arity: args.len() as u32,
            num_ctors: union.ctors.len() as u32,
        },
    }
}

fn canonicalize_expr(env: &mut Env, expr: &src::Expr) -> CResult<can::Expr> {
    let region = expr.region;
    let expr_ = match &expr.value {
        src::Expr_::Chr(c) => can::Expr_::Chr(*c),
        src::Expr_::Str(s) => can::Expr_::Str(s.clone()),
        src::Expr_::Int(n) => can::Expr_::Int(*n),
        src::Expr_::Float(f) => can::Expr_::Float(*f),
        src::Expr_::Var(var_type, name) => match var_type {
            src::VarType::LowVar => find_var(env, region, name)?,
            src::VarType::CapVar => match find_ctor(env, region, None, name) {
                Ok(info) => ctor_to_expr(info),
                Err(err) => record_alias_ctor(env, region, None, name).ok_or(err)?,
            },
        },
        src::Expr_::VarQual(var_type, qualifier, name) => match var_type {
            src::VarType::LowVar => find_qualified_var(env, region, qualifier, name)?,
            src::VarType::CapVar => match find_ctor(env, region, Some(qualifier), name) {
                Ok(info) => ctor_to_expr(info),
                Err(err) => {
                    record_alias_ctor(env, region, Some(qualifier), name).ok_or(err)?
                }
            },
        },
        src::Expr_::List(items) => can::Expr_::List(
            items
                .iter()
                .map(|e| canonicalize_expr(env, e))
                .collect::<CResult<Vec<_>>>()?,
        ),
        src::Expr_::Op(op) => {
            let entry = env.binops.get(op).ok_or_else(|| {
                Error::new(format!("I do not recognize the `{}` operator.", op), region)
            })?;
            if entry.home == env.module_name {
                can::Expr_::VarTopLevel(entry.function.clone())
            } else {
                can::Expr_::VarForeign(entry.home.clone(), entry.function.clone())
            }
        }
        src::Expr_::Negate(inner) => {
            can::Expr_::Negate(Box::new(canonicalize_expr(env, inner)?))
        }
        src::Expr_::Binops(pairs, last) => {
            let exprs_and_ops = pairs
                .iter()
                .map(|(e, op)| Ok((canonicalize_expr(env, e)?, op.clone())))
                .collect::<CResult<Vec<_>>>()?;
            let last = canonicalize_expr(env, last)?;
            return resolve_binops(env, exprs_and_ops, last);
        }
        src::Expr_::Lambda(args, body) => {
            let mut bound = Vec::new();
            let args = args
                .iter()
                .map(|p| canonicalize_pattern(env, p, &mut bound))
                .collect::<CResult<Vec<_>>>()?;
            env.scopes.push(bound.into_iter().collect());
            let body = canonicalize_expr(env, body);
            env.scopes.pop();
            can::Expr_::Lambda(args, Box::new(body?))
        }
        src::Expr_::Call(func, args) => can::Expr_::Call(
            Box::new(canonicalize_expr(env, func)?),
            args.iter()
                .map(|e| canonicalize_expr(env, e))
                .collect::<CResult<Vec<_>>>()?,
        ),
        src::Expr_::If(branches, otherwise) => can::Expr_::If(
            branches
                .iter()
                .map(|(c, b)| Ok((canonicalize_expr(env, c)?, canonicalize_expr(env, b)?)))
                .collect::<CResult<Vec<_>>>()?,
            Box::new(canonicalize_expr(env, otherwise)?),
        ),
        src::Expr_::Let(defs, body) => return canonicalize_let(env, region, defs, body),
        src::Expr_::Case(scrutinee, branches) => {
            let scrutinee = canonicalize_expr(env, scrutinee)?;
            let branches = branches
                .iter()
                .map(|(pattern, branch)| {
                    let mut bound = Vec::new();
                    let pattern = canonicalize_pattern(env, pattern, &mut bound)?;
                    env.scopes.push(bound.into_iter().collect());
                    let branch = canonicalize_expr(env, branch);
                    env.scopes.pop();
                    Ok((pattern, branch?))
                })
                .collect::<CResult<Vec<_>>>()?;
            can::Expr_::Case(Box::new(scrutinee), branches)
        }
        src::Expr_::Accessor(field) => can::Expr_::Accessor(field.clone()),
        src::Expr_::Access(record, field) => can::Expr_::Access(
            Box::new(canonicalize_expr(env, record)?),
            field.clone(),
        ),
        src::Expr_::Update(name, fields) => {
            let record = find_var(env, name.region, &name.value)?;
            can::Expr_::Update(
                Box::new(Located::new(name.region, record)),
                fields
                    .iter()
                    .map(|(field, e)| Ok((field.clone(), canonicalize_expr(env, e)?)))
                    .collect::<CResult<Vec<_>>>()?,
            )
        }
        src::Expr_::Record(fields) => {
            let mut seen = HashSet::new();
            for (field, _) in fields {
                if !seen.insert(field.value.clone()) {
                    return Err(Error::new(
                        format!("This record has the field `{}` more than once.", field.value),
                        field.region,
                    ));
                }
            }
            can::Expr_::Record(
                fields
                    .iter()
                    .map(|(field, e)| Ok((field.clone(), canonicalize_expr(env, e)?)))
                    .collect::<CResult<Vec<_>>>()?,
            )
        }
        src::Expr_::Unit => can::Expr_::Unit,
        src::Expr_::Shader(shader) => can::Expr_::Shader(shader.clone()),
        src::Expr_::Tuple(a, b, rest) => {
            if rest.len() > 1 {
                return Err(Error::new(
                    "Tuples can only hold two or three values. Use a record or a custom type instead.",
                    region,
                ));
            }
            can::Expr_::Tuple(
                Box::new(canonicalize_expr(env, a)?),
                Box::new(canonicalize_expr(env, b)?),
                rest.iter()
                    .map(|e| canonicalize_expr(env, e))
                    .collect::<CResult<Vec<_>>>()?,
            )
        }
    };
    Ok(Located::new(region, expr_))
}

fn ctor_to_expr(info: CtorInfo) -> can::Expr_ {
    can::Expr_::VarCtor(info.home, info.union, info.ctor)
}

/// A record type alias used as a constructor function: `Person "Ann" 40`.
/// Desugars to a lambda that builds the record, with arguments in field
/// declaration order.
fn record_alias_ctor(
    env: &Env,
    region: Region,
    qualifier: Option<&Name>,
    name: &Name,
) -> Option<can::Expr_> {
    let field_names: Vec<Name> = resolve_alias_record_fields(env, qualifier, name, 0)?;

    if field_names.is_empty() {
        return Some(can::Expr_::Record(vec![]));
    }
    // Every synthesized node must get a DISTINCT region: the type checker keys
    // inferred node types by region, so if the lambda, the record, and each
    // field-value var all reused the constructor's single region they would
    // clobber each other in `node_types` (e.g. a field's `number` type would
    // overwrite the record's type), corrupting the typed/native backend's
    // layout resolution. Nudge `end.col` by a per-node index to keep the
    // reported span near the constructor while making each region unique.
    let bump = |k: u32| {
        Region::new(
            region.start,
            Position::new(region.end.row, region.end.col.saturating_add(k)),
        )
    };
    let args: Vec<can::Pattern> = field_names
        .iter()
        .enumerate()
        .map(|(i, _)| {
            Located::new(bump(1 + i as u32), can::Pattern_::Var(Name::from(format!("_r{}", i))))
        })
        .collect();
    let n = field_names.len() as u32;
    let fields: Vec<(Located<Name>, can::Expr)> = field_names
        .iter()
        .enumerate()
        .map(|(i, field)| {
            (
                Located::new(region, field.clone()),
                Located::new(
                    bump(2 + n + i as u32),
                    can::Expr_::VarLocal(Name::from(format!("_r{}", i))),
                ),
            )
        })
        .collect();
    Some(can::Expr_::Lambda(
        args,
        Box::new(Located::new(bump(1 + n), can::Expr_::Record(fields))),
    ))
}

/// Field names of the record an alias resolves to, following alias chains
/// (`type alias Point = Draw.Point`, whose body is itself an alias for a
/// record). Returns None if the chain does not bottom out in a closed record.
fn resolve_alias_record_fields(
    env: &Env,
    qualifier: Option<&Name>,
    name: &Name,
    depth: u32,
) -> Option<Vec<Name>> {
    if depth > 20 {
        return None;
    }
    match qualifier {
        None => {
            if let Some((_, body)) = env.aliases.get(name) {
                record_field_names_src(env, body, depth)
            } else if let Some(module) = env.exposed_types.get(name) {
                record_field_names_can_alias(env, module, name, depth)
            } else {
                None
            }
        }
        Some(qualifier) => {
            let candidates = env.resolve_modules(qualifier);
            candidates.iter().find_map(|module| {
                if *module == env.module_name {
                    record_field_names_src(env, &env.aliases.get(name)?.1, depth)
                } else {
                    record_field_names_can_alias(env, module, name, depth)
                }
            })
        }
    }
}

/// Field names from a source-level alias body, chasing references to other
/// aliases (local or foreign) until a closed record is reached.
fn record_field_names_src(env: &Env, tipe: &src::Type, depth: u32) -> Option<Vec<Name>> {
    match &tipe.value {
        src::Type_::Record(fields, None) => {
            Some(fields.iter().map(|(n, _)| n.value.clone()).collect())
        }
        src::Type_::Type(_, name, _) => resolve_alias_record_fields(env, None, name, depth + 1),
        src::Type_::TypeQual(_, qualifier, name, _) => {
            resolve_alias_record_fields(env, Some(qualifier), name, depth + 1)
        }
        _ => None,
    }
}

/// Look up a foreign (interface or builtin) alias and resolve its record
/// fields, following further alias references in its canonical body.
fn record_field_names_can_alias(
    env: &Env,
    module: &Name,
    name: &Name,
    depth: u32,
) -> Option<Vec<Name>> {
    let body: can::Type = if let Some(interface) = env.interfaces.get(module) {
        interface.aliases.get(name).map(|(_, t)| t.clone())?
    } else if let Some((_, sig)) = builtins::lookup_alias(module.as_str(), name.as_str()) {
        builtins::parse_signature(sig)
    } else {
        return None;
    };
    record_field_names_can(env, &body, depth)
}

fn record_field_names_can(env: &Env, tipe: &can::Type, depth: u32) -> Option<Vec<Name>> {
    if depth > 20 {
        return None;
    }
    match tipe {
        can::Type::Record(fields, None) => Some(fields.iter().map(|(n, _)| n.clone()).collect()),
        can::Type::Type(module, name, _) => {
            record_field_names_can_alias(env, module, name, depth + 1)
        }
        _ => None,
    }
}

fn find_var(env: &Env, region: Region, name: &Name) -> CResult<can::Expr_> {
    if env.is_local(name) {
        return Ok(can::Expr_::VarLocal(name.clone()));
    }
    if env.top_level.contains(name) {
        return Ok(can::Expr_::VarTopLevel(name.clone()));
    }
    if let Some(module) = env.exposed_values.get(name) {
        return Ok(can::Expr_::VarForeign(module.clone(), name.clone()));
    }
    if builtins::lookup_exposed_value(name.as_str()).is_some() {
        return Ok(can::Expr_::VarForeign(Name::from("Basics"), name.clone()));
    }
    Err(Error::new(
        format!("I cannot find a `{}` variable.", name),
        region,
    ))
}

fn find_qualified_var(
    env: &Env,
    region: Region,
    qualifier: &Name,
    name: &Name,
) -> CResult<can::Expr_> {
    // Kernel modules are compiler-internal trusted JavaScript and may be
    // referenced fully-qualified without an explicit import (as elm/core and
    // elm-explorations/test do), so resolve them directly.
    if qualifier.as_str().starts_with("Elm.Kernel.") {
        return Ok(can::Expr_::VarForeign(qualifier.clone(), name.clone()));
    }
    let candidates = env.resolve_modules(qualifier);
    if candidates.is_empty() {
        return Err(Error::new(
            format!("I cannot find a module named `{}`.", qualifier),
            region,
        ));
    }
    for module in &candidates {
        if *module == env.module_name && env.top_level.contains(name) {
            return Ok(can::Expr_::VarTopLevel(name.clone()));
        }
        if module.as_str().starts_with("Elm.Kernel.") {
            return Ok(can::Expr_::VarForeign(module.clone(), name.clone()));
        }
        if builtins::lookup_value(module.as_str(), name.as_str()).is_some() {
            return Ok(can::Expr_::VarForeign(module.clone(), name.clone()));
        }
        if let Some(interface) = env.interfaces.get(module) {
            if interface.value_names.contains(name) {
                return Ok(can::Expr_::VarForeign(module.clone(), name.clone()));
            }
        }
    }
    Err(Error::new(
        format!(
            "The `{}` module does not expose a value named `{}`.",
            candidates[0], name
        ),
        region,
    ))
}

fn canonicalize_let(
    env: &mut Env,
    region: Region,
    defs: &[Located<src::Def>],
    body: &src::Expr,
) -> CResult<can::Expr> {
    // All names bound by the let block are in scope everywhere within it.
    let mut scope: HashSet<Name> = HashSet::new();
    for def in defs {
        match &def.value {
            src::Def::Define(name, ..) => {
                if !scope.insert(name.value.clone()) {
                    return Err(Error::new(
                        format!("This `let` defines `{}` more than once.", name.value),
                        name.region,
                    ));
                }
            }
            src::Def::Destruct(pattern, _) => {
                let mut bound = Vec::new();
                // Dry run purely to collect names; real canonicalization later.
                canonicalize_pattern(env, pattern, &mut bound)?;
                for name in bound {
                    if !scope.insert(name.clone()) {
                        return Err(Error::new(
                            format!("This `let` defines `{}` more than once.", name),
                            pattern.region,
                        ));
                    }
                }
            }
        }
    }
    env.scopes.push(scope);

    let result = (|| {
        let mut decls = Vec::new();
        for def in defs {
            match &def.value {
                src::Def::Define(name, args, def_body, annotation) => {
                    let annotation = match annotation {
                        Some(tipe) => Some(canonicalize_type(env, tipe)?),
                        None => None,
                    };
                    let mut bound = Vec::new();
                    let args = args
                        .iter()
                        .map(|p| canonicalize_pattern(env, p, &mut bound))
                        .collect::<CResult<Vec<_>>>()?;
                    env.scopes.push(bound.into_iter().collect());
                    let def_body = canonicalize_expr(env, def_body);
                    env.scopes.pop();
                    decls.push(can::LetDecl::Def(can::Def {
                        name: name.clone(),
                        args,
                        body: def_body?,
                        annotation,
                    }));
                }
                src::Def::Destruct(pattern, expr) => {
                    let mut bound = Vec::new();
                    let pattern = canonicalize_pattern(env, pattern, &mut bound)?;
                    let expr = canonicalize_expr(env, expr)?;
                    decls.push(can::LetDecl::Destruct(pattern, expr));
                }
            }
        }
        let body = canonicalize_expr(env, body)?;
        Ok((decls, body))
    })();

    env.scopes.pop();
    let (decls, body) = result?;
    let decls = sort_let_decls(decls)?;
    Ok(Located::new(region, can::Expr_::Let(decls, Box::new(body))))
}
