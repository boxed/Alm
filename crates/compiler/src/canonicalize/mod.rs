//! Port of `Canonicalize.*`: resolve names, rebuild binop chains with real
//! precedence, expand type aliases, and sort definitions into dependency
//! order (single definitions vs. mutually recursive function groups).

mod graph;

use std::collections::{HashMap, HashSet};

use crate::ast::canonical as can;
use crate::ast::source as src;
use crate::ast::source::Associativity;
use crate::builtins;
use crate::data::Name;
use crate::interface::{BinopDef, Interface, Interfaces};
use crate::reporting::{Located, Region};

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub region: Region,
}

impl Error {
    fn new(message: impl Into<String>, region: Region) -> Error {
        Error {
            message: message.into(),
            region,
        }
    }
}

type CResult<T> = Result<T, Error>;

#[derive(Clone)]
struct CtorInfo {
    home: Name,
    union: Name,
    ctor: can::Ctor,
}

struct Env<'a> {
    module_name: Name,
    top_level: HashSet<Name>,
    /// Locally declared unions and the builtin ones.
    ctors: HashMap<Name, CtorInfo>,
    local_types: HashSet<Name>,
    aliases: HashMap<Name, (Vec<Name>, src::Type)>,
    /// Modules reachable under which names (identity plus `as` aliases).
    /// One alias may point at several modules (`import A as X` twice).
    import_names: HashMap<Name, Vec<Name>>,
    /// Values exposed unqualified through `exposing` clauses on imports.
    exposed_values: HashMap<Name, Name>,
    /// Types exposed unqualified through `exposing` clauses on imports.
    exposed_types: HashMap<Name, Name>,
    /// Interfaces of already-compiled user modules.
    interfaces: &'a Interfaces,
    /// All operators usable in this module: builtins, imported customs,
    /// and locally defined ones.
    binops: HashMap<Name, BinopEntry>,
    scopes: Vec<HashSet<Name>>,
}

#[derive(Clone)]
struct BinopEntry {
    home: Name,
    function: Name,
    precedence: u8,
    associativity: Associativity,
}

impl Env<'_> {
    fn is_local(&self, name: &Name) -> bool {
        self.scopes.iter().rev().any(|scope| scope.contains(name))
    }

    fn resolve_modules(&self, name: &Name) -> Vec<Name> {
        self.import_names.get(name).cloned().unwrap_or_default()
    }
}

/// Canonicalize a single module with no user imports (single-file mode).
pub fn canonicalize(module: &src::Module) -> Result<can::Module, Vec<Error>> {
    let interfaces = Interfaces::new();
    canonicalize_module(module, &interfaces).map(|(canonical, _)| canonical)
}

pub fn canonicalize_module(
    module: &src::Module,
    interfaces: &Interfaces,
) -> Result<(can::Module, Interface), Vec<Error>> {
    let module_name = module.get_name();
    let mut errors = Vec::new();

    // Collect unions and constructors.
    let mut ctors: HashMap<Name, CtorInfo> = HashMap::new();
    let mut local_types: HashSet<Name> = HashSet::new();
    for union in &module.unions {
        local_types.insert(union.value.name.value.clone());
    }
    let mut aliases: HashMap<Name, (Vec<Name>, src::Type)> = HashMap::new();
    for alias in &module.aliases {
        let name = alias.value.name.value.clone();
        if local_types.contains(&name) || aliases.contains_key(&name) {
            errors.push(Error::new(
                format!("This module has multiple types named `{}`.", name),
                alias.value.name.region,
            ));
        }
        aliases.insert(
            name,
            (
                alias.value.vars.iter().map(|v| v.value.clone()).collect(),
                alias.value.tipe.clone(),
            ),
        );
    }

    for union in &module.unions {
        let union_name = union.value.name.value.clone();
        let num_ctors = union.value.ctors.len() as u32;
        for (index, (ctor_name, args)) in union.value.ctors.iter().enumerate() {
            let info = CtorInfo {
                home: module_name.clone(),
                union: union_name.clone(),
                ctor: can::Ctor {
                    name: ctor_name.value.clone(),
                    index: index as u32,
                    arity: args.len() as u32,
                    num_ctors,
                },
            };
            if ctors.insert(ctor_name.value.clone(), info).is_some() {
                errors.push(Error::new(
                    format!(
                        "This module defines the constructor `{}` more than once.",
                        ctor_name.value
                    ),
                    ctor_name.region,
                ));
            }
        }
    }

    // Operator table: builtins first, then imported and local operators.
    let mut binops: HashMap<Name, BinopEntry> = HashMap::new();
    for infix in builtins::INFIXES {
        binops.insert(
            Name::from(infix.op),
            BinopEntry {
                home: Name::from(infix.module),
                function: Name::from(infix.function),
                precedence: infix.precedence,
                associativity: infix.associativity,
            },
        );
    }
    for infix in &module.binops {
        if !module
            .values
            .iter()
            .any(|v| v.value.name.value == infix.value.function)
        {
            errors.push(Error::new(
                format!(
                    "The `{}` operator points at `{}`, but that function is not defined in this module.",
                    infix.value.op, infix.value.function
                ),
                infix.region,
            ));
            continue;
        }
        binops.insert(
            infix.value.op.clone(),
            BinopEntry {
                home: module_name.clone(),
                function: infix.value.function.clone(),
                precedence: infix.value.precedence,
                associativity: infix.value.associativity,
            },
        );
    }

    // Imports: identity names for builtin modules plus user aliases.
    let mut import_names: HashMap<Name, Vec<Name>> = HashMap::new();
    let mut add_import_name = |map: &mut HashMap<Name, Vec<Name>>, alias: Name, target: Name| {
        let entry = map.entry(alias).or_default();
        if !entry.contains(&target) {
            entry.push(target);
        }
    };
    let mut exposed_values: HashMap<Name, Name> = HashMap::new();
    let mut exposed_types: HashMap<Name, Name> = HashMap::new();
    for module_name in builtins::MODULES {
        add_import_name(&mut import_names, Name::from(*module_name), Name::from(*module_name));
    }
    // Elm's default imports include `import Platform.Cmd as Cmd` and
    // `import Platform.Sub as Sub`.
    add_import_name(&mut import_names, Name::from("Cmd"), Name::from("Platform.Cmd"));
    add_import_name(&mut import_names, Name::from("Sub"), Name::from("Platform.Sub"));
    for import in &module.imports {
        let import_name = &import.name.value;
        // Kernel modules are trusted JavaScript: importable, values untyped.
        if import_name.as_str().starts_with("Elm.Kernel.") {
            add_import_name(&mut import_names, import_name.clone(), import_name.clone());
            continue;
        }
        let is_builtin = builtins::is_builtin_module(import_name.as_str());
        let user_interface = interfaces.get(import_name);
        if !is_builtin && user_interface.is_none() {
            errors.push(Error::new(
                format!("I cannot find a module named `{}`.", import_name),
                import.name.region,
            ));
            continue;
        }
        add_import_name(&mut import_names, import_name.clone(), import_name.clone());
        if let Some(alias) = &import.alias {
            add_import_name(&mut import_names, alias.clone(), import_name.clone());
        }
        match &import.exposing {
            src::Exposing::Open => {
                if let Some(interface) = user_interface {
                    for name in &interface.value_names {
                        exposed_values.insert(name.clone(), import_name.clone());
                    }
                    for name in interface.unions.keys() {
                        exposed_types.insert(name.clone(), import_name.clone());
                    }
                    for name in interface.aliases.keys() {
                        exposed_types.insert(name.clone(), import_name.clone());
                    }
                    for union_name in &interface.open_unions {
                        expose_union_ctors(
                            &mut ctors,
                            import_name,
                            &interface.unions[union_name],
                        );
                    }
                    for (op, def) in &interface.binops {
                        binops.insert(
                            op.clone(),
                            BinopEntry {
                                home: import_name.clone(),
                                function: def.function.clone(),
                                precedence: def.precedence,
                                associativity: def.associativity,
                            },
                        );
                    }
                } else {
                    for value in builtins::values() {
                        if value.module == import_name.as_str() {
                            exposed_values.insert(Name::from(value.name), import_name.clone());
                        }
                    }
                    for union in builtins::UNIONS {
                        if union.module == import_name.as_str() {
                            exposed_types.insert(Name::from(union.name), import_name.clone());
                        }
                    }
                }
            }
            src::Exposing::Explicit(items) => {
                for item in items {
                    match item {
                        src::Exposed::Lower(name) => {
                            let exists = match user_interface {
                                Some(interface) => interface.value_names.contains(&name.value),
                                None => builtins::lookup_value(
                                    import_name.as_str(),
                                    name.value.as_str(),
                                )
                                .is_some(),
                            };
                            if exists {
                                exposed_values.insert(name.value.clone(), import_name.clone());
                            } else {
                                errors.push(Error::new(
                                    format!(
                                        "The `{}` module does not expose a value named `{}`.",
                                        import_name, name.value
                                    ),
                                    name.region,
                                ));
                            }
                        }
                        src::Exposed::Upper(name, privacy) => {
                            let open = matches!(privacy, src::Privacy::Public(_));
                            match user_interface {
                                Some(interface) => {
                                    if let Some(union) = interface.unions.get(&name.value) {
                                        exposed_types
                                            .insert(name.value.clone(), import_name.clone());
                                        if open {
                                            if interface.open_unions.contains(&name.value) {
                                                expose_union_ctors(
                                                    &mut ctors,
                                                    import_name,
                                                    union,
                                                );
                                            } else {
                                                errors.push(Error::new(
                                                    format!(
                                                        "The `{}` module exposes the `{}` type opaquely; its constructors are private.",
                                                        import_name, name.value
                                                    ),
                                                    name.region,
                                                ));
                                            }
                                        }
                                    } else if interface.aliases.contains_key(&name.value) {
                                        exposed_types
                                            .insert(name.value.clone(), import_name.clone());
                                    } else {
                                        errors.push(Error::new(
                                            format!(
                                                "The `{}` module does not expose a type named `{}`.",
                                                import_name, name.value
                                            ),
                                            name.region,
                                        ));
                                    }
                                }
                                None => {
                                    exposed_types
                                        .insert(name.value.clone(), import_name.clone());
                                    if open {
                                        if let Some((union, _)) = builtins::lookup_ctor_by_union(
                                            import_name.as_str(),
                                            name.value.as_str(),
                                        ) {
                                            for (index, _) in union.ctors.iter().enumerate() {
                                                let info =
                                                    builtin_ctor_info(union, index as u32);
                                                ctors.insert(info.ctor.name.clone(), info);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Builtin operators are globally available; custom
                        // ones come from the module's interface.
                        src::Exposed::Operator(region, op) => {
                            if let Some(interface) = user_interface {
                                match interface.binops.get(op) {
                                    Some(def) => {
                                        binops.insert(
                                            op.clone(),
                                            BinopEntry {
                                                home: import_name.clone(),
                                                function: def.function.clone(),
                                                precedence: def.precedence,
                                                associativity: def.associativity,
                                            },
                                        );
                                    }
                                    None => {
                                        errors.push(Error::new(
                                            format!(
                                                "The `{}` module does not expose a `{}` operator.",
                                                import_name, op
                                            ),
                                            *region,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Top-level names, with duplicate detection.
    let mut top_level: HashSet<Name> = HashSet::new();
    for value in &module.values {
        if !top_level.insert(value.value.name.value.clone()) {
            errors.push(Error::new(
                format!(
                    "This module has multiple definitions named `{}`.",
                    value.value.name.value
                ),
                value.value.name.region,
            ));
        }
    }
    for port in &module.ports {
        if !top_level.insert(port.name.value.clone()) {
            errors.push(Error::new(
                format!(
                    "This module has multiple definitions named `{}`.",
                    port.name.value
                ),
                port.name.region,
            ));
        }
    }

    let mut env = Env {
        module_name: module_name.clone(),
        top_level,
        ctors,
        local_types,
        aliases,
        import_names,
        exposed_values,
        exposed_types,
        interfaces,
        binops,
        scopes: vec![],
    };

    // Canonicalize unions.
    let mut unions = Vec::new();
    for union in &module.unions {
        match canonicalize_union(&env, &union.value) {
            Ok(u) => unions.push(u),
            Err(e) => errors.push(e),
        }
    }

    // Canonicalize top-level values.
    let mut defs = Vec::new();
    for value in &module.values {
        match canonicalize_value(&mut env, &value.value) {
            Ok(def) => defs.push(def),
            Err(e) => errors.push(e),
        }
    }

    // Canonicalize port declarations.
    let mut ports = Vec::new();
    for port in &module.ports {
        match canonicalize_type(&env, &port.tipe) {
            Ok(tipe) => ports.push(can::PortDecl {
                name: port.name.value.clone(),
                tipe,
            }),
            Err(e) => errors.push(e),
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // Dependency-sort the top-level definitions.
    let decls = match sort_defs(defs) {
        Ok(decls) => decls,
        Err(e) => return Err(vec![e]),
    };

    let canonical = can::Module {
        name: module_name,
        decls,
        unions,
        ports,
    };
    let interface = build_interface(&env, module, &canonical).map_err(|e| vec![e])?;
    Ok((canonical, interface))
}

/// Compute what this module exposes, validating the `exposing` list.
fn build_interface(
    env: &Env,
    module: &src::Module,
    canonical: &can::Module,
) -> CResult<Interface> {
    let mut interface = Interface::default();
    let unions_by_name: HashMap<Name, &can::Union> = canonical
        .unions
        .iter()
        .map(|u| (u.name.clone(), u))
        .collect();

    let expose_alias = |interface: &mut Interface, name: &Name| -> bool {
        if let Some((vars, body)) = env.aliases.get(name) {
            if let Ok(canonical_body) = canonicalize_type(env, body) {
                interface
                    .aliases
                    .insert(name.clone(), (vars.clone(), canonical_body));
                return true;
            }
        }
        false
    };

    match &module.exports.value {
        src::Exposing::Open => {
            for name in &env.top_level {
                interface.value_names.insert(name.clone());
            }
            for union in &canonical.unions {
                interface.unions.insert(union.name.clone(), union.clone());
                interface.open_unions.insert(union.name.clone());
            }
            let alias_names: Vec<Name> = env.aliases.keys().cloned().collect();
            for name in alias_names {
                expose_alias(&mut interface, &name);
            }
            for infix in &module.binops {
                interface.binops.insert(
                    infix.value.op.clone(),
                    BinopDef {
                        associativity: infix.value.associativity,
                        precedence: infix.value.precedence,
                        function: infix.value.function.clone(),
                        tipe: None,
                    },
                );
            }
        }
        src::Exposing::Explicit(items) => {
            for item in items {
                match item {
                    src::Exposed::Lower(name) => {
                        if env.top_level.contains(&name.value) {
                            interface.value_names.insert(name.value.clone());
                        } else {
                            return Err(Error::new(
                                format!(
                                    "You are trying to expose `{}`, but it is not defined in this module.",
                                    name.value
                                ),
                                name.region,
                            ));
                        }
                    }
                    src::Exposed::Upper(name, privacy) => {
                        if let Some(union) = unions_by_name.get(&name.value) {
                            interface
                                .unions
                                .insert(name.value.clone(), (*union).clone());
                            if matches!(privacy, src::Privacy::Public(_)) {
                                interface.open_unions.insert(name.value.clone());
                            }
                        } else if expose_alias(&mut interface, &name.value) {
                            if matches!(privacy, src::Privacy::Public(_)) {
                                return Err(Error::new(
                                    format!(
                                        "`{}` is a type alias; expose it as `{}` without `(..)`.",
                                        name.value, name.value
                                    ),
                                    name.region,
                                ));
                            }
                        } else {
                            return Err(Error::new(
                                format!(
                                    "You are trying to expose a type named `{}`, but it is not defined in this module.",
                                    name.value
                                ),
                                name.region,
                            ));
                        }
                    }
                    src::Exposed::Operator(region, op) => {
                        match module.binops.iter().find(|i| i.value.op == *op) {
                            Some(infix) => {
                                interface.binops.insert(
                                    op.clone(),
                                    BinopDef {
                                        associativity: infix.value.associativity,
                                        precedence: infix.value.precedence,
                                        function: infix.value.function.clone(),
                                        tipe: None,
                                    },
                                );
                            }
                            None => {
                                return Err(Error::new(
                                    format!(
                                        "You are trying to expose the `{}` operator, but it is not defined in this module.",
                                        op
                                    ),
                                    *region,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(interface)
}

fn expose_union_ctors(ctors: &mut HashMap<Name, CtorInfo>, home: &Name, union: &can::Union) {
    let num_ctors = union.ctors.len() as u32;
    for ctor in &union.ctors {
        ctors.entry(ctor.name.clone()).or_insert_with(|| CtorInfo {
            home: home.clone(),
            union: union.name.clone(),
            ctor: can::Ctor {
                name: ctor.name.clone(),
                index: ctor.index,
                arity: ctor.args.len() as u32,
                num_ctors,
            },
        });
    }
}

// DEPENDENCY SORTING

fn def_is_function(def: &can::Def) -> bool {
    !def.args.is_empty() || matches!(def.body.value, can::Expr_::Lambda(..))
}

fn sort_defs(defs: Vec<can::Def>) -> Result<Vec<can::DeclGroup>, Error> {
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
        if directly_cyclic[i] {
            return Err(Error::new(
                format!(
                    "The value `{}` is defined in terms of itself, and it is not a function, so it would run forever.",
                    def.name.value
                ),
                def.name.region,
            ));
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

/// Which nodes sit on a cycle (including self-loops) in the given graph?
fn cyclic_nodes(count: usize, edges: &[Vec<usize>]) -> Vec<bool> {
    let components = graph::strongly_connected_components(count, edges);
    let mut cyclic = vec![false; count];
    for component in components {
        if component.len() > 1 {
            for i in component {
                cyclic[i] = true;
            }
        } else {
            let i = component[0];
            if edges[i].contains(&i) {
                cyclic[i] = true;
            }
        }
    }
    cyclic
}

/// Like `collect_refs`, but stops at lambdas and function definitions:
/// those references only run when called, so they cannot make evaluation
/// of a value diverge.
fn collect_direct_refs(expr: &can::Expr, refs: &mut HashSet<Name>) {
    use can::Expr_::*;
    match &expr.value {
        Lambda(..) => {}
        Let(decls, body) => {
            for decl in decls {
                match decl {
                    can::LetDecl::Def(def) => {
                        if def.args.is_empty() {
                            collect_direct_refs(&def.body, refs);
                        }
                    }
                    can::LetDecl::Recursive(inner) => {
                        for def in inner {
                            if def.args.is_empty() {
                                collect_direct_refs(&def.body, refs);
                            }
                        }
                    }
                    can::LetDecl::Destruct(_, e) => collect_direct_refs(e, refs),
                }
            }
            collect_direct_refs(body, refs);
        }
        VarLocal(name) | VarTopLevel(name) => {
            refs.insert(name.clone());
        }
        Binop(_, _, function, left, right) => {
            refs.insert(function.clone());
            collect_direct_refs(left, refs);
            collect_direct_refs(right, refs);
        }
        VarForeign(..) | VarCtor(..) | Chr(_) | Str(_) | Int(_) | Float(_) | Accessor(_)
        | Unit => {}
        List(items) => items.iter().for_each(|e| collect_direct_refs(e, refs)),
        Negate(e) => collect_direct_refs(e, refs),
        Call(f, args) => {
            collect_direct_refs(f, refs);
            args.iter().for_each(|e| collect_direct_refs(e, refs));
        }
        If(branches, otherwise) => {
            for (c, b) in branches {
                collect_direct_refs(c, refs);
                collect_direct_refs(b, refs);
            }
            collect_direct_refs(otherwise, refs);
        }
        Case(scrutinee, branches) => {
            collect_direct_refs(scrutinee, refs);
            for (_, b) in branches {
                collect_direct_refs(b, refs);
            }
        }
        Access(e, _) => collect_direct_refs(e, refs),
        Update(e, fields) => {
            collect_direct_refs(e, refs);
            fields.iter().for_each(|(_, e)| collect_direct_refs(e, refs));
        }
        Record(fields) => fields
            .iter()
            .for_each(|(_, e)| collect_direct_refs(e, refs)),
        Tuple(a, b, rest) => {
            collect_direct_refs(a, refs);
            collect_direct_refs(b, refs);
            rest.iter().for_each(|e| collect_direct_refs(e, refs));
        }
    }
}

/// Collect names of local/top-level variables referenced in an expression.
fn collect_refs(expr: &can::Expr, refs: &mut HashSet<Name>) {
    use can::Expr_::*;
    match &expr.value {
        VarLocal(name) | VarTopLevel(name) => {
            refs.insert(name.clone());
        }
        VarForeign(..) | VarCtor(..) | Chr(_) | Str(_) | Int(_) | Float(_) | Accessor(_)
        | Unit => {}
        List(items) => items.iter().for_each(|e| collect_refs(e, refs)),
        Negate(e) => collect_refs(e, refs),
        Binop(_, _, function, left, right) => {
            // A locally-defined operator's function is a real dependency.
            refs.insert(function.clone());
            collect_refs(left, refs);
            collect_refs(right, refs);
        }
        Lambda(_, body) => collect_refs(body, refs),
        Call(f, args) => {
            collect_refs(f, refs);
            args.iter().for_each(|e| collect_refs(e, refs));
        }
        If(branches, otherwise) => {
            for (c, b) in branches {
                collect_refs(c, refs);
                collect_refs(b, refs);
            }
            collect_refs(otherwise, refs);
        }
        Let(decls, body) => {
            for decl in decls {
                match decl {
                    can::LetDecl::Def(def) => collect_refs(&def.body, refs),
                    can::LetDecl::Recursive(defs) => {
                        defs.iter().for_each(|d| collect_refs(&d.body, refs))
                    }
                    can::LetDecl::Destruct(_, e) => collect_refs(e, refs),
                }
            }
            collect_refs(body, refs);
        }
        Case(scrutinee, branches) => {
            collect_refs(scrutinee, refs);
            for (_, b) in branches {
                collect_refs(b, refs);
            }
        }
        Access(e, _) => collect_refs(e, refs),
        Update(e, fields) => {
            collect_refs(e, refs);
            fields.iter().for_each(|(_, e)| collect_refs(e, refs));
        }
        Record(fields) => fields.iter().for_each(|(_, e)| collect_refs(e, refs)),
        Tuple(a, b, rest) => {
            collect_refs(a, refs);
            collect_refs(b, refs);
            rest.iter().for_each(|e| collect_refs(e, refs));
        }
    }
}

// UNIONS

fn canonicalize_union(env: &Env, union: &src::Union) -> CResult<can::Union> {
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

// TYPES

fn canonicalize_type(env: &Env, tipe: &src::Type) -> CResult<can::Type> {
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
            Ok(can::Type::Record(fields, ext.as_ref().map(|e| e.value.clone())))
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

// VALUES

fn canonicalize_value(env: &mut Env, value: &src::Value) -> CResult<can::Def> {
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

// PATTERNS

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

fn builtin_ctor_info(union: &'static builtins::BuiltinUnion, index: u32) -> CtorInfo {
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

// EXPRESSIONS

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
            return resolve_binops(env, exprs_and_ops, last, region);
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
    let field_names: Vec<Name> = match qualifier {
        None => {
            if let Some((_, body)) = env.aliases.get(name) {
                record_field_names_src(body)?
            } else if let Some(module) = env.exposed_types.get(name) {
                record_alias_fields_foreign(env, module, name)?
            } else {
                return None;
            }
        }
        Some(qualifier) => {
            let candidates = env.resolve_modules(qualifier);
            candidates.iter().find_map(|module| {
                if *module == env.module_name {
                    record_field_names_src(&env.aliases.get(name)?.1)
                } else {
                    record_alias_fields_foreign(env, module, name)
                }
            })?
        }
    };

    if field_names.is_empty() {
        return Some(can::Expr_::Record(vec![]));
    }
    let args: Vec<can::Pattern> = field_names
        .iter()
        .enumerate()
        .map(|(i, _)| {
            Located::new(region, can::Pattern_::Var(Name::from(format!("_r{}", i))))
        })
        .collect();
    let fields: Vec<(Located<Name>, can::Expr)> = field_names
        .iter()
        .enumerate()
        .map(|(i, field)| {
            (
                Located::new(region, field.clone()),
                Located::new(
                    region,
                    can::Expr_::VarLocal(Name::from(format!("_r{}", i))),
                ),
            )
        })
        .collect();
    Some(can::Expr_::Lambda(
        args,
        Box::new(Located::new(region, can::Expr_::Record(fields))),
    ))
}

fn record_field_names_src(tipe: &src::Type) -> Option<Vec<Name>> {
    match &tipe.value {
        src::Type_::Record(fields, None) => {
            Some(fields.iter().map(|(n, _)| n.value.clone()).collect())
        }
        _ => None,
    }
}

fn record_alias_fields_foreign(env: &Env, module: &Name, name: &Name) -> Option<Vec<Name>> {
    if let Some(interface) = env.interfaces.get(module) {
        if let Some((_, can::Type::Record(fields, None))) = interface.aliases.get(name) {
            return Some(fields.iter().map(|(n, _)| n.clone()).collect());
        }
    }
    if let Some((_, body)) = builtins::lookup_alias(module.as_str(), name.as_str()) {
        if let can::Type::Record(fields, None) = builtins::parse_signature(body) {
            return Some(fields.iter().map(|(n, _)| n.clone()).collect());
        }
    }
    None
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

// LET

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

/// Sort let declarations into dependency order, grouping mutually recursive
/// functions. Cycles are only legal among function definitions.
fn sort_let_decls(decls: Vec<can::LetDecl>) -> CResult<Vec<can::LetDecl>> {
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
        if directly_cyclic[i] {
            let (message, region) = match decl {
                can::LetDecl::Def(def) => (
                    format!(
                        "The value `{}` is defined in terms of itself, and it is not a function.",
                        def.name.value
                    ),
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
                    "This destructuring is part of a definition cycle.".to_string(),
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

fn pattern_names(pattern: &can::Pattern) -> Vec<Name> {
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

// BINOP RESOLUTION — port of the precedence/associativity logic in
// Canonicalize.Expression.

struct OpInfo {
    op: Name,
    home: Name,
    function: Name,
    precedence: u8,
    associativity: Associativity,
    region: Region,
}

fn resolve_binops(
    env: &Env,
    pairs: Vec<(can::Expr, Located<Name>)>,
    last: can::Expr,
    region: Region,
) -> CResult<can::Expr> {
    let mut exprs = Vec::new();
    let mut ops = Vec::new();
    for (expr, op) in pairs {
        let entry = env.binops.get(&op.value).ok_or_else(|| {
            Error::new(
                format!("I do not recognize the `{}` operator.", op.value),
                op.region,
            )
        })?;
        exprs.push(expr);
        ops.push(OpInfo {
            op: op.value.clone(),
            home: entry.home.clone(),
            function: entry.function.clone(),
            precedence: entry.precedence,
            associativity: entry.associativity,
            region: op.region,
        });
    }
    exprs.push(last);

    let mut pos = 0;
    let result = climb(&mut exprs.into_iter().map(Some).collect(), &ops, &mut pos, 0)?;
    debug_assert_eq!(pos, ops.len());
    let _ = region;
    Ok(result)
}

fn climb(
    exprs: &mut Vec<Option<can::Expr>>,
    ops: &[OpInfo],
    pos: &mut usize,
    min_precedence: u8,
) -> CResult<can::Expr> {
    let mut lhs = exprs[*pos].take().unwrap();
    while *pos < ops.len() && ops[*pos].precedence >= min_precedence {
        let op_index = *pos;
        let op = &ops[op_index];
        *pos += 1;

        // Everything binding tighter than this operator goes into the rhs.
        let next_min = match op.associativity {
            Associativity::Left | Associativity::Non => op.precedence + 1,
            Associativity::Right => op.precedence,
        };
        // The rhs starts at the expression slot just after this operator.
        let rhs = climb_rhs(exprs, ops, pos, next_min)?;

        if op.associativity == Associativity::Non
            && *pos < ops.len()
            && ops[*pos].precedence == op.precedence
            && ops[*pos].associativity == Associativity::Non
        {
            return Err(Error::new(
                format!(
                    "You cannot chain the non-associative operators `{}` and `{}` without parentheses.",
                    op.op, ops[*pos].op
                ),
                ops[*pos].region,
            ));
        }

        let op_region = lhs.region.merge(rhs.region);
        lhs = Located::new(
            op_region,
            can::Expr_::Binop(
                op.op.clone(),
                op.home.clone(),
                op.function.clone(),
                Box::new(lhs),
                Box::new(rhs),
            ),
        );
    }
    Ok(lhs)
}

fn climb_rhs(
    exprs: &mut Vec<Option<can::Expr>>,
    ops: &[OpInfo],
    pos: &mut usize,
    min_precedence: u8,
) -> CResult<can::Expr> {
    climb(exprs, ops, pos, min_precedence)
}
