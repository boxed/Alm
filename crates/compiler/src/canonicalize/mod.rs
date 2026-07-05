//! Port of `Canonicalize.*`: resolve names, rebuild binop chains with real
//! precedence, expand type aliases, and sort definitions into dependency
//! order (single definitions vs. mutually recursive function groups).

mod binops;
mod expr;
mod graph;
mod sort;
mod types;

use binops::resolve_binops;
use expr::{builtin_ctor_info, canonicalize_value};
use sort::{sort_defs, sort_let_decls};
use types::{canonicalize_type, canonicalize_union};
pub(crate) use types::subst_can_type;

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
    let add_import_name = |map: &mut HashMap<Name, Vec<Name>>, alias: Name, target: Name| {
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
                            // `exposing (..)` also exposes each union's constructors.
                            for (index, _) in union.ctors.iter().enumerate() {
                                let info = builtin_ctor_info(union, index as u32);
                                ctors.insert(info.ctor.name.clone(), info);
                            }
                        }
                    }
                    for (module, name, _, _) in builtins::ALIASES {
                        if *module == import_name.as_str() {
                            exposed_types.insert(Name::from(*name), import_name.clone());
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
                                        if let Some(union) = builtins::lookup_ctor_by_union(
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


// UNIONS

// TYPES

// VALUES

// PATTERNS

// EXPRESSIONS

// LET

// BINOP RESOLUTION — port of the precedence/associativity logic in
// Canonicalize.Expression.

