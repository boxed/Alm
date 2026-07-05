//! Type canonicalization: resolving type constructors, expanding
//! aliases (local, foreign, and builtin), and union declarations.

use super::*;

pub(super) fn canonicalize_union(env: &Env, union: &src::Union) -> CResult<can::Union> {
    let vars: Vec<Name> = union.vars.iter().map(|v| v.value.clone()).collect();
    let mut ctors = Vec::new();
    for (index, (name, args)) in union.ctors.iter().enumerate() {
        let args = args
            .iter()
            .map(|t| canonicalize_type(env, t))
            .collect::<CResult<Vec<_>>>()?;
        ctors.push(can::UnionCtor {
            name: name.value.clone(),
            index: index as u32,
            args,
            region: name.region,
        });
    }
    Ok(can::Union {
        name: union.name.value.clone(),
        vars,
        ctors,
    })
}

pub(super) fn canonicalize_type(env: &Env, tipe: &src::Type) -> CResult<can::Type> {
    canonicalize_type_help(env, tipe, &HashMap::new(), 0)
}

fn canonicalize_type_help(
    env: &Env,
    tipe: &src::Type,
    substitutions: &HashMap<Name, can::Type>,
    depth: u32,
) -> CResult<can::Type> {
    if depth > 100 {
        return Err(Error::new(
            "This type alias is recursive. Type aliases cannot refer to themselves; use a custom `type` instead.",
            tipe.region,
        ));
    }
    match &tipe.value {
        src::Type_::Lambda(arg, result) => Ok(can::Type::Lambda(
            Box::new(canonicalize_type_help(env, arg, substitutions, depth)?),
            Box::new(canonicalize_type_help(env, result, substitutions, depth)?),
        )),
        src::Type_::Var(name) => Ok(substitutions
            .get(name)
            .cloned()
            .unwrap_or_else(|| can::Type::Var(name.clone()))),
        src::Type_::Type(region, name, args) => {
            let args = args
                .iter()
                .map(|t| canonicalize_type_help(env, t, substitutions, depth))
                .collect::<CResult<Vec<_>>>()?;
            resolve_type(env, *region, None, name, args, depth)
        }
        src::Type_::TypeQual(region, qualifier, name, args) => {
            let args = args
                .iter()
                .map(|t| canonicalize_type_help(env, t, substitutions, depth))
                .collect::<CResult<Vec<_>>>()?;
            resolve_type(env, *region, Some(qualifier), name, args, depth)
        }
        src::Type_::Record(fields, ext) => {
            let fields = fields
                .iter()
                .map(|(name, t)| {
                    Ok((
                        name.value.clone(),
                        canonicalize_type_help(env, t, substitutions, depth)?,
                    ))
                })
                .collect::<CResult<Vec<_>>>()?;
            let ext_name = ext.as_ref().map(|e| e.value.clone());
            // If the extension variable is being substituted (e.g. an alias
            // `{ base | ... }` applied with a concrete record for `base`),
            // merge the substituted record's fields in, mirroring subst_can_type.
            match ext_name.as_ref().and_then(|e| substitutions.get(e)) {
                Some(can::Type::Record(more_fields, ext2)) => {
                    let mut merged = fields;
                    merged.extend(more_fields.iter().cloned());
                    Ok(can::Type::Record(merged, ext2.clone()))
                }
                Some(can::Type::Var(n)) => Ok(can::Type::Record(fields, Some(n.clone()))),
                _ => Ok(can::Type::Record(fields, ext_name)),
            }
        }
        src::Type_::Unit => Ok(can::Type::Unit),
        src::Type_::Tuple(a, b, rest) => {
            if rest.len() > 1 {
                return Err(Error::new(
                    "Tuples can only hold two or three values.",
                    tipe.region,
                ));
            }
            Ok(can::Type::Tuple(
                Box::new(canonicalize_type_help(env, a, substitutions, depth)?),
                Box::new(canonicalize_type_help(env, b, substitutions, depth)?),
                match rest.first() {
                    Some(t) => Some(Box::new(canonicalize_type_help(env, t, substitutions, depth)?)),
                    None => None,
                },
            ))
        }
    }
}

fn resolve_type(
    env: &Env,
    region: Region,
    qualifier: Option<&Name>,
    name: &Name,
    args: Vec<can::Type>,
    depth: u32,
) -> CResult<can::Type> {
    match qualifier {
        None => {
            if env.local_types.contains(name) {
                return Ok(can::Type::Type(env.module_name.clone(), name.clone(), args));
            }
            if let Some((vars, body)) = env.aliases.get(name) {
                return expand_alias(env, region, name, vars, body, args, depth);
            }
            if let Some(module) = env.exposed_types.get(name) {
                return resolve_foreign_type(env, region, module, name, args);
            }
            if let Some(home) = builtins::lookup_type_home(name.as_str()) {
                return Ok(can::Type::Type(Name::from(home), name.clone(), args));
            }
            Err(Error::new(
                format!("I cannot find a type named `{}`.", name),
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
            let mut last_err = None;
            for module in &candidates {
                if *module == env.module_name {
                    match resolve_type(env, region, None, name, args.clone(), depth) {
                        Ok(t) => return Ok(t),
                        Err(e) => {
                            last_err = Some(e);
                            continue;
                        }
                    }
                }
                if builtins::lookup_type_home(name.as_str()) == Some(module.as_str()) {
                    return Ok(can::Type::Type(module.clone(), name.clone(), args));
                }
                match resolve_foreign_type(env, region, module, name, args.clone()) {
                    Ok(t) => return Ok(t),
                    Err(e) => last_err = Some(e),
                }
            }
            Err(last_err.unwrap())
        }
    }
}

/// A type that lives in another user module: an exported union or alias.
fn resolve_foreign_type(
    env: &Env,
    region: Region,
    module: &Name,
    name: &Name,
    args: Vec<can::Type>,
) -> CResult<can::Type> {
    // Builtin aliases like Http.Metadata or Url.Url.
    if let Some((vars, body)) = builtins::lookup_alias(module.as_str(), name.as_str()) {
        if vars.len() != args.len() {
            return Err(Error::new(
                format!(
                    "The `{}` type alias needs {} argument{}, but I see {}.",
                    name,
                    vars.len(),
                    if vars.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                region,
            ));
        }
        let expanded = builtins::parse_signature(body);
        let map: HashMap<Name, can::Type> =
            vars.iter().map(|v| Name::from(*v)).zip(args).collect();
        return Ok(subst_can_type(&expanded, &map));
    }
    // Builtin types addressed by module, e.g. `Http.Error` or `Html.Html`.
    if builtins::is_builtin_type(module.as_str(), name.as_str()) {
        return Ok(can::Type::Type(module.clone(), name.clone(), args));
    }
    let interface = env.interfaces.get(module).ok_or_else(|| {
        Error::new(
            format!("The `{}` module does not have a type named `{}`.", module, name),
            region,
        )
    })?;
    if let Some(union) = interface.unions.get(name) {
        if union.vars.len() != args.len() {
            return Err(Error::new(
                format!(
                    "The `{}` type needs {} argument{}, but I see {}.",
                    name,
                    union.vars.len(),
                    if union.vars.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                region,
            ));
        }
        return Ok(can::Type::Type(module.clone(), name.clone(), args));
    }
    if let Some((vars, body)) = interface.aliases.get(name) {
        if vars.len() != args.len() {
            return Err(Error::new(
                format!(
                    "The `{}` type alias needs {} argument{}, but I see {}.",
                    name,
                    vars.len(),
                    if vars.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                region,
            ));
        }
        let map: HashMap<Name, can::Type> = vars.iter().cloned().zip(args).collect();
        return Ok(subst_can_type(body, &map));
    }
    Err(Error::new(
        format!("The `{}` module does not have a type named `{}`.", module, name),
        region,
    ))
}

/// Substitute type variables in an already-canonical type (used to expand
/// aliases exported by other modules).
pub(crate) fn subst_can_type(tipe: &can::Type, map: &HashMap<Name, can::Type>) -> can::Type {
    match tipe {
        can::Type::Var(name) => map.get(name).cloned().unwrap_or_else(|| tipe.clone()),
        can::Type::Lambda(arg, result) => can::Type::Lambda(
            Box::new(subst_can_type(arg, map)),
            Box::new(subst_can_type(result, map)),
        ),
        can::Type::Type(home, name, args) => can::Type::Type(
            home.clone(),
            name.clone(),
            args.iter().map(|a| subst_can_type(a, map)).collect(),
        ),
        can::Type::Record(fields, ext) => {
            let new_fields: Vec<(Name, can::Type)> = fields
                .iter()
                .map(|(n, t)| (n.clone(), subst_can_type(t, map)))
                .collect();
            match ext.as_ref().and_then(|e| map.get(e)) {
                None => can::Type::Record(new_fields, ext.clone()),
                Some(can::Type::Var(n)) => can::Type::Record(new_fields, Some(n.clone())),
                Some(can::Type::Record(more_fields, ext2)) => {
                    let mut merged = new_fields;
                    merged.extend(more_fields.iter().cloned());
                    can::Type::Record(merged, ext2.clone())
                }
                Some(_) => can::Type::Record(new_fields, ext.clone()),
            }
        }
        can::Type::Unit => can::Type::Unit,
        can::Type::Tuple(a, b, c) => can::Type::Tuple(
            Box::new(subst_can_type(a, map)),
            Box::new(subst_can_type(b, map)),
            c.as_ref().map(|c| Box::new(subst_can_type(c, map))),
        ),
    }
}

fn expand_alias(
    env: &Env,
    region: Region,
    name: &Name,
    vars: &[Name],
    body: &src::Type,
    args: Vec<can::Type>,
    depth: u32,
) -> CResult<can::Type> {
    if vars.len() != args.len() {
        return Err(Error::new(
            format!(
                "The `{}` type alias needs {} argument{}, but I see {}.",
                name,
                vars.len(),
                if vars.len() == 1 { "" } else { "s" },
                args.len()
            ),
            region,
        ));
    }
    let substitutions: HashMap<Name, can::Type> =
        vars.iter().cloned().zip(args).collect();
    canonicalize_type_help(env, body, &substitutions, depth + 1)
}
