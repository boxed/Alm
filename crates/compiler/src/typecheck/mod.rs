//! Port of `Type.Constrain.*` and `Type.Solve` — Hindley-Milner inference.
//!
//! The Haskell compiler builds a constraint tree and solves it with
//! rank-based generalization. alm infers directly (Algorithm W style),
//! generalizing at each dependency-sorted definition group, which produces
//! the same types for the same programs.

pub mod types;

use std::collections::{HashMap, HashSet};

use crate::ast::canonical as can;
use crate::builtins;
use crate::data::Name;
use crate::interface::Interfaces;
use crate::reporting::Region;
use types::{Content, FlatType, Pool, Super, Variable};

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub region: Region,
}

/// A polymorphic type. `free` maps type variable names that must NOT be
/// re-instantiated (because they are shared with the enclosing scope) to
/// their live pool variables; every other variable in `tipe` is quantified.
#[derive(Clone)]
struct Scheme {
    tipe: can::Type,
    free: HashMap<Name, Variable>,
}

impl Scheme {
    fn closed(tipe: can::Type) -> Scheme {
        Scheme {
            tipe,
            free: HashMap::new(),
        }
    }
}

#[derive(Clone)]
enum Binding {
    Scheme(Scheme),
    Mono(Variable),
}

struct UnionInfo {
    vars: Vec<Name>,
    ctor_args: HashMap<Name, Vec<can::Type>>,
}

pub struct Checker<'a> {
    pool: Pool,
    globals: HashMap<Name, Binding>,
    scopes: Vec<HashMap<Name, Binding>>,
    unions: HashMap<(Name, Name), UnionInfo>,
    interfaces: &'a Interfaces,
    /// Rigid type variables introduced by enclosing annotations; inner
    /// annotations reuse them so `a` means the same thing throughout.
    rigid_scope: Vec<HashMap<Name, Variable>>,
    errors: Vec<Error>,
}

/// Check a single module with no user imports (single-file mode).
pub fn check(module: &can::Module) -> Result<(), Vec<Error>> {
    let interfaces = Interfaces::new();
    check_module(module, &interfaces).map(|_| ())
}

/// Check a module against the interfaces of its dependencies. Returns the
/// inferred type of every top-level definition.
pub fn check_module(
    module: &can::Module,
    interfaces: &Interfaces,
) -> Result<HashMap<Name, can::Type>, Vec<Error>> {
    let mut unions = HashMap::new();
    for union in &module.unions {
        unions.insert(
            (module.name.clone(), union.name.clone()),
            UnionInfo {
                vars: union.vars.clone(),
                ctor_args: union
                    .ctors
                    .iter()
                    .map(|c| (c.name.clone(), c.args.clone()))
                    .collect(),
            },
        );
    }
    for (module_name, interface) in interfaces {
        for union in interface.unions.values() {
            unions.insert(
                (module_name.clone(), union.name.clone()),
                UnionInfo {
                    vars: union.vars.clone(),
                    ctor_args: union
                        .ctors
                        .iter()
                        .map(|c| (c.name.clone(), c.args.clone()))
                        .collect(),
                },
            );
        }
    }
    for union in builtins::UNIONS {
        unions.insert(
            (Name::from(union.module), Name::from(union.name)),
            UnionInfo {
                vars: union.vars.iter().map(|v| Name::from(*v)).collect(),
                ctor_args: union
                    .ctors
                    .iter()
                    .map(|(name, args)| {
                        (
                            Name::from(*name),
                            args.iter().map(|a| builtins::parse_signature(a)).collect(),
                        )
                    })
                    .collect(),
            },
        );
    }

    let mut checker = Checker {
        pool: Pool::new(),
        globals: HashMap::new(),
        scopes: Vec::new(),
        unions,
        interfaces,
        rigid_scope: Vec::new(),
        errors: Vec::new(),
    };

    for port in &module.ports {
        checker.globals.insert(
            port.name.clone(),
            Binding::Scheme(Scheme::closed(port.tipe.clone())),
        );
    }

    for group in &module.decls {
        checker.check_decl_group(group);
    }

    if checker.errors.is_empty() {
        let mut types = HashMap::new();
        for (name, binding) in &checker.globals {
            if let Binding::Scheme(scheme) = binding {
                types.insert(name.clone(), scheme.tipe.clone());
            }
        }
        Ok(types)
    } else {
        Err(checker.errors)
    }
}

type Infer<T> = Result<T, Error>;

impl Checker<'_> {
    // ENVIRONMENT

    fn lookup(&self, name: &Name) -> Option<&Binding> {
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.get(name) {
                return Some(binding);
            }
        }
        self.globals.get(name)
    }

    fn bind_local(&mut self, name: Name, binding: Binding) {
        self.scopes
            .last_mut()
            .expect("bind_local requires an open scope")
            .insert(name, binding);
    }

    /// Pool variables that appear in monomorphic bindings currently in
    /// scope; generalization must not quantify them.
    fn env_free_vars(&mut self) -> HashSet<Variable> {
        let mut mono_vars = Vec::new();
        for scope in &self.scopes {
            for binding in scope.values() {
                match binding {
                    Binding::Mono(var) => mono_vars.push(*var),
                    Binding::Scheme(scheme) => {
                        mono_vars.extend(scheme.free.values().copied())
                    }
                }
            }
        }
        for binding in self.globals.values() {
            if let Binding::Mono(var) = binding {
                mono_vars.push(*var);
            }
        }
        for scope in &self.rigid_scope {
            mono_vars.extend(scope.values().copied());
        }
        let mut free = HashSet::new();
        for var in mono_vars {
            self.collect_free(var, &mut free);
        }
        free
    }

    fn collect_free(&mut self, var: Variable, free: &mut HashSet<Variable>) {
        let root = self.pool.find(var);
        if !free.insert(root) {
            return;
        }
        if let Content::Structure(flat) = self.pool.content(root) {
            for child in types::flat_children(&flat) {
                self.collect_free(child, free);
            }
        }
    }

    // DECLARATION GROUPS

    fn check_decl_group(&mut self, group: &can::DeclGroup) {
        match group {
            can::DeclGroup::Value(def) => {
                let name = def.name.value.clone();
                match self.check_def(def, None) {
                    Ok(scheme) => {
                        self.globals.insert(name, Binding::Scheme(scheme));
                    }
                    Err(error) => {
                        self.errors.push(error);
                        self.globals.insert(
                            name,
                            Binding::Scheme(Scheme::closed(can::Type::Var(Name::from("a")))),
                        );
                    }
                }
            }
            can::DeclGroup::Recursive(defs) => {
                let result = self.check_recursive_defs(defs, |checker, name, binding| {
                    checker.globals.insert(name, binding);
                });
                if let Err(error) = result {
                    self.errors.push(error);
                    for def in defs {
                        self.globals.insert(
                            def.name.value.clone(),
                            Binding::Scheme(Scheme::closed(can::Type::Var(Name::from("a")))),
                        );
                    }
                }
            }
        }
    }

    /// Check a group of mutually recursive function definitions:
    /// pre-bind every name, infer every body, then generalize together.
    fn check_recursive_defs(
        &mut self,
        defs: &[can::Def],
        mut bind: impl FnMut(&mut Checker, Name, Binding),
    ) -> Infer<()> {
        // Pre-bind: annotated defs get their annotation as a scheme
        // (recursion may use them polymorphically); unannotated defs get a
        // monomorphic variable.
        let mut mono_vars = Vec::new();
        self.scopes.push(HashMap::new());
        for def in defs {
            let binding = match &def.annotation {
                Some(annotation) => {
                    Binding::Scheme(Scheme::closed(annotation.clone()))
                }
                None => {
                    let var = self.pool.fresh_var();
                    mono_vars.push(Some(var));
                    Binding::Mono(var)
                }
            };
            if def.annotation.is_some() {
                mono_vars.push(None);
            }
            self.bind_local(def.name.value.clone(), binding);
        }

        let result = (|| {
            for (def, mono) in defs.iter().zip(&mono_vars) {
                let def_type = self.infer_def_type(def)?;
                if let Some(var) = mono {
                    self.unify(*var, def_type, def.name.region, || {
                        format!("The definition of `{}`", def.name.value)
                    })?;
                }
            }
            Ok(())
        })();

        self.scopes.pop();
        result?;

        // Generalize after the whole group.
        for (def, mono) in defs.iter().zip(&mono_vars) {
            let scheme = match (&def.annotation, mono) {
                (Some(annotation), _) => Scheme::closed(annotation.clone()),
                (None, Some(var)) => self.generalize(*var),
                (None, None) => unreachable!(),
            };
            bind(self, def.name.value.clone(), Binding::Scheme(scheme));
        }
        Ok(())
    }

    /// Infer (and, when annotated, check) the type of one definition.
    /// Returns the generalizable scheme for non-recursive bindings.
    fn check_def(&mut self, def: &can::Def, _hint: Option<()>) -> Infer<Scheme> {
        let def_type = self.infer_def_type(def)?;
        match &def.annotation {
            Some(annotation) => Ok(Scheme::closed(annotation.clone())),
            None => Ok(self.generalize(def_type)),
        }
    }

    /// Infer the full type of a definition (args -> body), checking it
    /// against the annotation when there is one.
    fn infer_def_type(&mut self, def: &can::Def) -> Infer<Variable> {
        self.scopes.push(HashMap::new());

        // Instantiate the annotation first so its rigid variables are in
        // scope for annotations on inner `let` definitions.
        let expected = def.annotation.as_ref().map(|annotation| {
            let mut substitutions = self.flatten_rigid_scope();
            let expected = self.type_to_variable(annotation, &mut substitutions, true);
            self.rigid_scope.push(substitutions);
            expected
        });

        let result = (|| {
            let mut arg_vars = Vec::new();
            for arg in &def.args {
                arg_vars.push(self.infer_pattern(arg)?);
            }
            let body_var = self.infer_expr(&def.body)?;
            let mut def_type = body_var;
            for arg in arg_vars.into_iter().rev() {
                def_type = self.pool.fresh(Content::Structure(FlatType::Fun(arg, def_type)));
            }
            if let Some(expected) = expected {
                self.unify(expected, def_type, def.name.region, || {
                    format!(
                        "Something is off with the body of the `{}` definition",
                        def.name.value
                    )
                })?;
            }
            Ok(def_type)
        })();

        if def.annotation.is_some() {
            self.rigid_scope.pop();
        }
        self.scopes.pop();
        result
    }

    fn flatten_rigid_scope(&self) -> HashMap<Name, Variable> {
        let mut merged = HashMap::new();
        for scope in &self.rigid_scope {
            for (name, var) in scope {
                merged.insert(name.clone(), *var);
            }
        }
        merged
    }

    // INSTANTIATION

    /// Instantiate a scheme with fresh flexible variables (rigid for
    /// annotation checking). Variable names carry Elm's pseudo-typeclass
    /// constraints: `number*`, `comparable*`, `appendable*`, `compappend*`.
    fn instantiate(&mut self, scheme: &Scheme) -> Variable {
        let mut substitutions = scheme.free.clone();
        self.type_to_variable(&scheme.tipe, &mut substitutions, false)
    }

    fn instantiate_rigid(&mut self, tipe: &can::Type) -> Variable {
        let mut substitutions = HashMap::new();
        self.type_to_variable(tipe, &mut substitutions, true)
    }

    fn type_to_variable(
        &mut self,
        tipe: &can::Type,
        substitutions: &mut HashMap<Name, Variable>,
        rigid: bool,
    ) -> Variable {
        match tipe {
            can::Type::Var(name) => {
                if let Some(var) = substitutions.get(name) {
                    return *var;
                }
                let content = match (var_super(name), rigid) {
                    (Some(super_), false) => Content::FlexSuper(super_, Some(name.clone())),
                    (Some(super_), true) => Content::RigidSuper(super_, name.clone()),
                    (None, false) => Content::FlexVar(Some(name.clone())),
                    (None, true) => Content::RigidVar(name.clone()),
                };
                let var = self.pool.fresh(content);
                substitutions.insert(name.clone(), var);
                var
            }
            can::Type::Lambda(arg, result) => {
                let arg = self.type_to_variable(arg, substitutions, rigid);
                let result = self.type_to_variable(result, substitutions, rigid);
                self.pool.fresh(Content::Structure(FlatType::Fun(arg, result)))
            }
            can::Type::Type(home, name, args) => {
                let args = args
                    .iter()
                    .map(|a| self.type_to_variable(a, substitutions, rigid))
                    .collect();
                self.pool.fresh(Content::Structure(FlatType::App(
                    home.clone(),
                    name.clone(),
                    args,
                )))
            }
            can::Type::Record(fields, ext) => {
                let fields = fields
                    .iter()
                    .map(|(name, t)| {
                        (name.clone(), self.type_to_variable(t, substitutions, rigid))
                    })
                    .collect();
                let ext = match ext {
                    Some(name) => {
                        if let Some(var) = substitutions.get(name) {
                            *var
                        } else {
                            let content = if rigid {
                                Content::RigidVar(name.clone())
                            } else {
                                Content::FlexVar(Some(name.clone()))
                            };
                            let var = self.pool.fresh(content);
                            substitutions.insert(name.clone(), var);
                            var
                        }
                    }
                    None => self.pool.fresh(Content::Structure(FlatType::EmptyRecord)),
                };
                self.pool
                    .fresh(Content::Structure(FlatType::Record(fields, ext)))
            }
            can::Type::Unit => self.pool.fresh(Content::Structure(FlatType::Unit)),
            can::Type::Tuple(a, b, c) => {
                let a = self.type_to_variable(a, substitutions, rigid);
                let b = self.type_to_variable(b, substitutions, rigid);
                let c = c
                    .as_ref()
                    .map(|c| self.type_to_variable(c, substitutions, rigid));
                self.pool
                    .fresh(Content::Structure(FlatType::Tuple(a, b, c)))
            }
        }
    }

    // GENERALIZATION

    /// Turn an inferred type into a scheme, quantifying every flexible
    /// variable that is not shared with the enclosing environment.
    fn generalize(&mut self, var: Variable) -> Scheme {
        let env_free = self.env_free_vars();
        let mut state = GeneralizeState {
            names: HashMap::new(),
            free: HashMap::new(),
            counter: 0,
        };
        let tipe = self.variable_to_type(var, &env_free, &mut state);
        Scheme {
            tipe,
            free: state.free,
        }
    }

    fn variable_to_type(
        &mut self,
        var: Variable,
        env_free: &HashSet<Variable>,
        state: &mut GeneralizeState,
    ) -> can::Type {
        let root = self.pool.find(var);
        if let Some(name) = state.names.get(&root) {
            return can::Type::Var(name.clone());
        }
        match self.pool.content(root) {
            Content::Structure(flat) => match flat {
                FlatType::App(home, name, args) => can::Type::Type(
                    home,
                    name,
                    args.iter()
                        .map(|&a| self.variable_to_type(a, env_free, state))
                        .collect(),
                ),
                FlatType::Fun(arg, result) => can::Type::Lambda(
                    Box::new(self.variable_to_type(arg, env_free, state)),
                    Box::new(self.variable_to_type(result, env_free, state)),
                ),
                FlatType::EmptyRecord => can::Type::Record(vec![], None),
                FlatType::Record(..) => {
                    let (fields, ext) = self.pool.gather_fields_public(root);
                    let fields = fields
                        .into_iter()
                        .map(|(name, v)| (name, self.variable_to_type(v, env_free, state)))
                        .collect();
                    let ext = match self.pool.content(ext) {
                        Content::Structure(FlatType::EmptyRecord) => None,
                        _ => match self.variable_to_type(ext, env_free, state) {
                            can::Type::Var(name) => Some(name),
                            _ => None,
                        },
                    };
                    can::Type::Record(fields, ext)
                }
                FlatType::Unit => can::Type::Unit,
                FlatType::Tuple(a, b, c) => can::Type::Tuple(
                    Box::new(self.variable_to_type(a, env_free, state)),
                    Box::new(self.variable_to_type(b, env_free, state)),
                    c.map(|c| Box::new(self.variable_to_type(c, env_free, state))),
                ),
            },
            content => {
                let name = state.fresh_name(root, &content);
                if env_free.contains(&root) {
                    state.free.insert(name.clone(), root);
                }
                state.names.insert(root, name.clone());
                can::Type::Var(name)
            }
        }
    }

    // UNIFICATION WRAPPER

    fn unify(
        &mut self,
        expected: Variable,
        actual: Variable,
        region: Region,
        context: impl FnOnce() -> String,
    ) -> Infer<()> {
        match self.pool.unify(expected, actual) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Re-render both sides from clean copies for the message
                // (unification may have partially merged them).
                let expected_str = self.pool.render(expected);
                let actual_str = self.pool.render(actual);
                self.pool.set_content(expected, Content::Error);
                self.pool.set_content(actual, Content::Error);
                Err(Error {
                    message: format!(
                        "{}.\n\nI needed:\n\n    {}\n\nBut I found:\n\n    {}",
                        context(),
                        expected_str,
                        actual_str
                    ),
                    region,
                })
            }
        }
    }

    // PATTERNS

    /// Infer the type of a pattern, binding its variables monomorphically
    /// in the current scope.
    fn infer_pattern(&mut self, pattern: &can::Pattern) -> Infer<Variable> {
        use can::Pattern_::*;
        match &pattern.value {
            Anything => Ok(self.pool.fresh_var()),
            Var(name) => {
                let var = self.pool.fresh_var();
                self.bind_local(name.clone(), Binding::Mono(var));
                Ok(var)
            }
            Alias(inner, name) => {
                let var = self.infer_pattern(inner)?;
                self.bind_local(name.value.clone(), Binding::Mono(var));
                Ok(var)
            }
            Unit => Ok(self.pool.fresh(Content::Structure(FlatType::Unit))),
            Chr(_) => Ok(self.app("Char", "Char", vec![])),
            Str(_) => Ok(self.app("String", "String", vec![])),
            Int(_) => Ok(self.pool.fresh(Content::FlexSuper(Super::Number, None))),
            Tuple(a, b, rest) => {
                let a = self.infer_pattern(a)?;
                let b = self.infer_pattern(b)?;
                let c = match rest.first() {
                    Some(p) => Some(self.infer_pattern(p)?),
                    None => None,
                };
                Ok(self.pool.fresh(Content::Structure(FlatType::Tuple(a, b, c))))
            }
            List(items) => {
                let elem = self.pool.fresh_var();
                for item in items {
                    let item_var = self.infer_pattern(item)?;
                    self.unify(elem, item_var, item.region, || {
                        "All entries of a list pattern must have the same type".to_string()
                    })?;
                }
                Ok(self.app("List", "List", vec![elem]))
            }
            Cons(head, tail) => {
                let head_var = self.infer_pattern(head)?;
                let list_var = self.app("List", "List", vec![head_var]);
                let tail_var = self.infer_pattern(tail)?;
                self.unify(list_var, tail_var, tail.region, || {
                    "The tail of a `::` pattern must be a list of the head's type".to_string()
                })?;
                Ok(list_var)
            }
            Record(fields) => {
                let mut field_types = std::collections::BTreeMap::new();
                for field in fields {
                    let var = self.pool.fresh_var();
                    self.bind_local(field.value.clone(), Binding::Mono(var));
                    field_types.insert(field.value.clone(), var);
                }
                let ext = self.pool.fresh_var();
                Ok(self
                    .pool
                    .fresh(Content::Structure(FlatType::Record(field_types, ext))))
            }
            Ctor(home, union_name, ctor, args) => {
                let (result_var, arg_vars) =
                    self.instantiate_ctor(home, union_name, &ctor.name, pattern.region)?;
                debug_assert_eq!(arg_vars.len(), args.len());
                for (arg_pattern, expected) in args.iter().zip(arg_vars) {
                    let actual = self.infer_pattern(arg_pattern)?;
                    self.unify(expected, actual, arg_pattern.region, || {
                        format!("This argument to the `{}` pattern", ctor.name)
                    })?;
                }
                Ok(result_var)
            }
        }
    }

    /// Instantiate a constructor: returns (result type, argument types).
    fn instantiate_ctor(
        &mut self,
        home: &Name,
        union_name: &Name,
        ctor_name: &Name,
        region: Region,
    ) -> Infer<(Variable, Vec<Variable>)> {
        let info = self
            .unions
            .get(&(home.clone(), union_name.clone()))
            .ok_or_else(|| Error {
                message: format!("I cannot find the `{}` type.", union_name),
                region,
            })?;
        let vars = info.vars.clone();
        let arg_types = info.ctor_args.get(ctor_name).cloned().ok_or_else(|| Error {
            message: format!("The `{}` type has no `{}` constructor.", union_name, ctor_name),
            region,
        })?;

        let mut substitutions: HashMap<Name, Variable> = HashMap::new();
        let union_args: Vec<Variable> = vars
            .iter()
            .map(|v| {
                let var = self.pool.fresh_var();
                substitutions.insert(v.clone(), var);
                var
            })
            .collect();
        let arg_vars = arg_types
            .iter()
            .map(|t| self.type_to_variable(t, &mut substitutions, false))
            .collect();
        let result = self.pool.fresh(Content::Structure(FlatType::App(
            home.clone(),
            union_name.clone(),
            union_args,
        )));
        Ok((result, arg_vars))
    }

    fn app(&mut self, home: &str, name: &str, args: Vec<Variable>) -> Variable {
        self.pool.fresh(Content::Structure(FlatType::App(
            Name::from(home),
            Name::from(name),
            args,
        )))
    }

    // EXPRESSIONS

    fn infer_expr(&mut self, expr: &can::Expr) -> Infer<Variable> {
        use can::Expr_::*;
        let region = expr.region;
        match &expr.value {
            Chr(_) => Ok(self.app("Char", "Char", vec![])),
            Str(_) => Ok(self.app("String", "String", vec![])),
            Int(_) => Ok(self.pool.fresh(Content::FlexSuper(Super::Number, None))),
            Float(_) => Ok(self.app("Basics", "Float", vec![])),
            VarLocal(name) | VarTopLevel(name) => {
                let binding = self.lookup(name).cloned().ok_or_else(|| Error {
                    message: format!("I cannot find a `{}` variable.", name),
                    region,
                })?;
                Ok(match binding {
                    Binding::Mono(var) => var,
                    Binding::Scheme(scheme) => self.instantiate(&scheme),
                })
            }
            VarForeign(module, name) => {
                // Kernel values are trusted: they get a fresh flexible type.
                if module.as_str().starts_with("Elm.Kernel.") {
                    return Ok(self.pool.fresh_var());
                }
                let tipe = match builtins::lookup_value(module.as_str(), name.as_str()) {
                    Some(value) => builtins::parse_signature(value.signature),
                    None => self
                        .interfaces
                        .get(module)
                        .and_then(|interface| interface.values.get(name))
                        .cloned()
                        .ok_or_else(|| Error {
                            message: format!("I cannot find `{}.{}`.", module, name),
                            region,
                        })?,
                };
                Ok(self.instantiate(&Scheme::closed(tipe)))
            }
            VarCtor(home, union_name, ctor) => {
                let (result, args) =
                    self.instantiate_ctor(home, union_name, &ctor.name, region)?;
                let mut tipe = result;
                for arg in args.into_iter().rev() {
                    tipe = self.pool.fresh(Content::Structure(FlatType::Fun(arg, tipe)));
                }
                Ok(tipe)
            }
            List(items) => {
                let elem = self.pool.fresh_var();
                for item in items {
                    let item_var = self.infer_expr(item)?;
                    self.unify(elem, item_var, item.region, || {
                        "All entries of a list must have the same type".to_string()
                    })?;
                }
                Ok(self.app("List", "List", vec![elem]))
            }
            Negate(inner) => {
                let inner_var = self.infer_expr(inner)?;
                let number = self.pool.fresh(Content::FlexSuper(Super::Number, None));
                self.unify(number, inner_var, inner.region, || {
                    "I can only negate Int and Float values".to_string()
                })?;
                Ok(number)
            }
            Binop(op, home, function, left, right) => {
                let value = builtins::lookup_value(home.as_str(), function.as_str())
                    .unwrap_or_else(|| panic!("unknown binop function {}.{}", home, function));
                let tipe = builtins::parse_signature(value.signature);
                let func_var = self.instantiate(&Scheme::closed(tipe));
                let left_var = self.infer_expr(left)?;
                let right_var = self.infer_expr(right)?;
                let result = self.pool.fresh_var();
                let expected_right = self
                    .pool
                    .fresh(Content::Structure(FlatType::Fun(right_var, result)));
                let expected = self
                    .pool
                    .fresh(Content::Structure(FlatType::Fun(left_var, expected_right)));
                self.unify(func_var, expected, region, || {
                    format!("The arguments to the `{}` operator are off", op)
                })?;
                Ok(result)
            }
            Lambda(args, body) => {
                self.scopes.push(HashMap::new());
                let result = (|| {
                    let mut arg_vars = Vec::new();
                    for arg in args {
                        arg_vars.push(self.infer_pattern(arg)?);
                    }
                    let body_var = self.infer_expr(body)?;
                    let mut tipe = body_var;
                    for arg in arg_vars.into_iter().rev() {
                        tipe = self.pool.fresh(Content::Structure(FlatType::Fun(arg, tipe)));
                    }
                    Ok(tipe)
                })();
                self.scopes.pop();
                result
            }
            Call(func, args) => {
                let func_var = self.infer_expr(func)?;
                let mut arg_vars = Vec::new();
                for arg in args {
                    arg_vars.push(self.infer_expr(arg)?);
                }
                let result = self.pool.fresh_var();
                let mut expected = result;
                for arg in arg_vars.into_iter().rev() {
                    expected = self
                        .pool
                        .fresh(Content::Structure(FlatType::Fun(arg, expected)));
                }
                self.unify(func_var, expected, region, || {
                    "This function call has a problem".to_string()
                })?;
                Ok(result)
            }
            If(branches, otherwise) => {
                let result = self.pool.fresh_var();
                let bool_type = self.app("Basics", "Bool", vec![]);
                for (condition, branch) in branches {
                    let cond_var = self.infer_expr(condition)?;
                    self.unify(bool_type, cond_var, condition.region, || {
                        "This `if` condition must be a Bool".to_string()
                    })?;
                    let branch_var = self.infer_expr(branch)?;
                    self.unify(result, branch_var, branch.region, || {
                        "All branches of this `if` must have the same type".to_string()
                    })?;
                }
                let else_var = self.infer_expr(otherwise)?;
                self.unify(result, else_var, otherwise.region, || {
                    "All branches of this `if` must have the same type".to_string()
                })?;
                Ok(result)
            }
            Let(decls, body) => {
                self.scopes.push(HashMap::new());
                let result = (|| {
                    for decl in decls {
                        match decl {
                            can::LetDecl::Def(def) => {
                                let scheme = self.check_def(def, None)?;
                                self.bind_local(
                                    def.name.value.clone(),
                                    Binding::Scheme(scheme),
                                );
                            }
                            can::LetDecl::Recursive(defs) => {
                                self.check_recursive_defs(defs, |checker, name, binding| {
                                    checker.bind_local(name, binding);
                                })?;
                            }
                            can::LetDecl::Destruct(pattern, value) => {
                                let value_var = self.infer_expr(value)?;
                                let pattern_var = self.infer_pattern(pattern)?;
                                self.unify(pattern_var, value_var, pattern.region, || {
                                    "This destructuring pattern does not match the value"
                                        .to_string()
                                })?;
                            }
                        }
                    }
                    self.infer_expr(body)
                })();
                self.scopes.pop();
                result
            }
            Case(scrutinee, branches) => {
                let scrutinee_var = self.infer_expr(scrutinee)?;
                let result = self.pool.fresh_var();
                for (pattern, branch) in branches {
                    self.scopes.push(HashMap::new());
                    let branch_result = (|| {
                        let pattern_var = self.infer_pattern(pattern)?;
                        self.unify(scrutinee_var, pattern_var, pattern.region, || {
                            "This pattern does not match the type of the value being inspected"
                                .to_string()
                        })?;
                        let branch_var = self.infer_expr(branch)?;
                        self.unify(result, branch_var, branch.region, || {
                            "All branches of this `case` must have the same type".to_string()
                        })
                    })();
                    self.scopes.pop();
                    branch_result?;
                }
                Ok(result)
            }
            Accessor(field) => {
                let field_var = self.pool.fresh_var();
                let ext = self.pool.fresh_var();
                let mut fields = std::collections::BTreeMap::new();
                fields.insert(field.clone(), field_var);
                let record = self
                    .pool
                    .fresh(Content::Structure(FlatType::Record(fields, ext)));
                Ok(self
                    .pool
                    .fresh(Content::Structure(FlatType::Fun(record, field_var))))
            }
            Access(record, field) => {
                let record_var = self.infer_expr(record)?;
                let field_var = self.pool.fresh_var();
                let ext = self.pool.fresh_var();
                let mut fields = std::collections::BTreeMap::new();
                fields.insert(field.value.clone(), field_var);
                let expected = self
                    .pool
                    .fresh(Content::Structure(FlatType::Record(fields, ext)));
                self.unify(expected, record_var, field.region, || {
                    format!("This is not a record with a `{}` field", field.value)
                })?;
                Ok(field_var)
            }
            Update(record, fields) => {
                let record_var = self.infer_expr(record)?;
                let mut field_types = std::collections::BTreeMap::new();
                let mut value_pairs = Vec::new();
                for (field, value) in fields {
                    let field_var = self.pool.fresh_var();
                    field_types.insert(field.value.clone(), field_var);
                    value_pairs.push((field_var, value));
                }
                let ext = self.pool.fresh_var();
                let expected = self
                    .pool
                    .fresh(Content::Structure(FlatType::Record(field_types, ext)));
                self.unify(expected, record_var, region, || {
                    "This record update mentions fields the record does not have".to_string()
                })?;
                for (field_var, value) in value_pairs {
                    let value_var = self.infer_expr(value)?;
                    self.unify(field_var, value_var, value.region, || {
                        "Record updates cannot change the type of a field".to_string()
                    })?;
                }
                Ok(record_var)
            }
            Record(fields) => {
                let mut field_types = std::collections::BTreeMap::new();
                for (field, value) in fields {
                    let value_var = self.infer_expr(value)?;
                    field_types.insert(field.value.clone(), value_var);
                }
                let empty = self.pool.fresh(Content::Structure(FlatType::EmptyRecord));
                Ok(self
                    .pool
                    .fresh(Content::Structure(FlatType::Record(field_types, empty))))
            }
            Unit => Ok(self.pool.fresh(Content::Structure(FlatType::Unit))),
            Tuple(a, b, rest) => {
                let a = self.infer_expr(a)?;
                let b = self.infer_expr(b)?;
                let c = match rest.first() {
                    Some(e) => Some(self.infer_expr(e)?),
                    None => None,
                };
                Ok(self.pool.fresh(Content::Structure(FlatType::Tuple(a, b, c))))
            }
        }
    }
}

/// Elm's rule: a type variable's name determines its constraint.
fn var_super(name: &Name) -> Option<Super> {
    let name = name.as_str();
    if name.starts_with("number") {
        Some(Super::Number)
    } else if name.starts_with("comparable") {
        Some(Super::Comparable)
    } else if name.starts_with("appendable") {
        Some(Super::Appendable)
    } else if name.starts_with("compappend") {
        Some(Super::CompAppend)
    } else {
        None
    }
}

struct GeneralizeState {
    names: HashMap<Variable, Name>,
    free: HashMap<Name, Variable>,
    counter: usize,
}

impl GeneralizeState {
    fn fresh_name(&mut self, _var: Variable, content: &Content) -> Name {
        match content {
            Content::RigidVar(name) => name.clone(),
            Content::RigidSuper(_, name) => name.clone(),
            Content::FlexVar(Some(name)) if !self.names.values().any(|n| n == name) => {
                name.clone()
            }
            Content::FlexSuper(super_, _) => {
                let base = super_.name();
                let mut i = 0;
                loop {
                    let candidate = if i == 0 {
                        Name::from(base)
                    } else {
                        Name::from(format!("{}{}", base, i + 1))
                    };
                    if !self.names.values().any(|n| *n == candidate) {
                        return candidate;
                    }
                    i += 1;
                }
            }
            _ => {
                let mut n = self.counter;
                self.counter += 1;
                let mut name = String::new();
                loop {
                    name.insert(0, (b'a' + (n % 26) as u8) as char);
                    n /= 26;
                    if n == 0 {
                        break;
                    }
                    n -= 1;
                }
                // Avoid collisions with names already taken.
                while self.names.values().any(|existing| existing.as_str() == name) {
                    name.push('_');
                }
                Name::from(name)
            }
        }
    }
}
