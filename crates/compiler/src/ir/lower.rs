//! Lowering: canonical AST → backend-neutral IR.
//!
//! See the module docs in `ir` for what lowering does. The pattern
//! compilation and tail-call detection deliberately mirror the JS
//! backend (`generate`), so both backends agree on semantics.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::ast::canonical as can;
use crate::data::Name;

use super::{Branch, Expr, Function, GlobalValue, PrimOp, Program, Step, Test};

pub fn lower_project(modules: &[can::Module]) -> Program {
    let mut arities: HashMap<(Name, Name), usize> = HashMap::new();
    for module in modules {
        for group in &module.decls {
            for def in group_defs(group) {
                arities.insert(
                    (module.name.clone(), def.name.value.clone()),
                    def_arity(def),
                );
            }
        }
    }

    let mut lowerer = Lowerer {
        arities,
        module: Name::from(""),
        current_global: String::new(),
        counter: 0,
        functions: Vec::new(),
        values: Vec::new(),
        synthesized: HashSet::new(),
    };

    for module in modules {
        lowerer.module = module.name.clone();
        for group in &module.decls {
            for def in group_defs(group) {
                lowerer.top_level_def(def);
            }
        }
    }

    // The entry module is last in dependency order; expose its `main`
    // when it is a plain value.
    let main = modules.last().and_then(|module| {
        module.decls.iter().flat_map(group_defs).find_map(|def| {
            (def.name.value.as_str() == "main" && def_arity(def) == 0)
                .then(|| global_name(&module.name, &def.name.value))
        })
    });

    Program {
        functions: lowerer.functions,
        values: lowerer.values,
        main,
    }
}

fn group_defs(group: &can::DeclGroup) -> &[can::Def] {
    match group {
        can::DeclGroup::Value(def) => std::slice::from_ref(def),
        can::DeclGroup::Recursive(defs) => defs,
    }
}

/// The number of arguments a definition takes, seeing through
/// `f = \a b -> ...` the same way the JS backend does.
fn def_arity(def: &can::Def) -> usize {
    if !def.args.is_empty() {
        return def.args.len();
    }
    if let can::Expr_::Lambda(args, _) = &def.body.value {
        return args.len();
    }
    0
}

/// A definition's argument patterns and function body, if it is a function.
fn def_function(def: &can::Def) -> Option<(&[can::Pattern], &can::Expr)> {
    if !def.args.is_empty() {
        return Some((&def.args, &def.body));
    }
    if let can::Expr_::Lambda(args, body) = &def.body.value {
        return Some((args, body));
    }
    None
}

fn mangle_module(name: &Name) -> String {
    format!("${}", name.as_str().replace('.', "$"))
}

fn global_name(module: &Name, name: &Name) -> String {
    format!("{}${}", mangle_module(module), name)
}

/// How a definition refers to itself in its own body.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelfKind {
    TopLevel,
    Local,
}

/// Whether saturated self-calls in tail position should become `TailCall`.
enum TailCtx {
    No,
    Loop {
        name: Name,
        kind: SelfKind,
        arity: usize,
    },
}

struct Lowerer {
    /// Arity of every user-defined top-level definition, across modules.
    arities: HashMap<(Name, Name), usize>,
    module: Name,
    /// Global name of the top-level definition being lowered; lifted
    /// functions are named under it.
    current_global: String,
    counter: usize,
    functions: Vec<Function>,
    values: Vec<GlobalValue>,
    /// Constructor wrappers and field accessors already emitted.
    synthesized: HashSet<String>,
}

impl Lowerer {
    fn fresh(&mut self, suffix: &str) -> String {
        self.counter += 1;
        format!("{}${}{}", self.current_global, suffix, self.counter)
    }

    fn fresh_temp(&mut self) -> String {
        self.counter += 1;
        format!("$v{}", self.counter)
    }

    fn top_level_def(&mut self, def: &can::Def) {
        let name = global_name(&self.module.clone(), &def.name.value);
        self.current_global = name.clone();
        match def_function(def) {
            None => {
                let body = self.expr(&def.body, &TailCtx::No, false);
                self.values.push(GlobalValue { name, body });
            }
            Some((args, body)) => {
                self.function(
                    name,
                    Vec::new(),
                    args,
                    body,
                    Some((def.name.value.clone(), SelfKind::TopLevel)),
                );
            }
        }
    }

    /// Emit a function: captures first, then the argument patterns.
    /// When `self_ref` is given and the body tail-calls itself, the
    /// function is marked tail-recursive and those calls become `TailCall`.
    fn function(
        &mut self,
        name: String,
        captures: Vec<Name>,
        args: &[can::Pattern],
        body: &can::Expr,
        self_ref: Option<(Name, SelfKind)>,
    ) {
        let num_captures = captures.len();
        let mut params: Vec<String> = captures.iter().map(|c| c.to_string()).collect();
        let mut bindings: Vec<(String, Expr)> = Vec::new();
        for arg in args {
            match &arg.value {
                can::Pattern_::Var(name) => params.push(name.to_string()),
                _ => {
                    let temp = self.fresh_temp();
                    let mut tests = Vec::new();
                    self.pattern_tests(
                        arg,
                        &Expr::Local(temp.clone()),
                        &mut tests,
                        &mut bindings,
                    );
                    // Argument patterns are irrefutable: no tests (except
                    // single-constructor matches, which produce none).
                    params.push(temp);
                }
            }
        }

        let tail_recursive = self_ref.as_ref().is_some_and(|(self_name, kind)| {
            has_self_tail_call(self_name, *kind, args.len(), body)
        });
        let tail = if tail_recursive {
            let (self_name, kind) = self_ref.unwrap();
            TailCtx::Loop {
                name: self_name,
                kind,
                arity: args.len(),
            }
        } else {
            TailCtx::No
        };

        let mut lowered = self.expr(body, &tail, true);
        for (binding_name, path) in bindings.into_iter().rev() {
            lowered = Expr::Let {
                name: binding_name,
                value: Box::new(path),
                body: Box::new(lowered),
            };
        }

        self.functions.push(Function {
            name,
            params,
            captures: num_captures,
            tail_recursive,
            body: lowered,
        });
    }

    // EXPRESSIONS
    //
    // `is_tail` tracks whether the expression sits in tail position of the
    // function being lowered; only there may self-calls become `TailCall`.

    fn expr(&mut self, expr: &can::Expr, tail: &TailCtx, is_tail: bool) -> Expr {
        use can::Expr_::*;
        match &expr.value {
            Chr(c) => Expr::Chr(*c),
            Str(s) => Expr::Str(s.clone()),
            Int(n) => Expr::Int(*n),
            Float(f) => Expr::Float(*f),
            Unit => Expr::Unit,
            // GLSL shaders target WebGL, which the native backend has no
            // runtime for; they are only reachable through the JS backend.
            Shader(_) => Expr::Crash("GLSL shaders are not supported by the native backend".to_string()),
            VarLocal(name) => Expr::Local(name.to_string()),
            VarTopLevel(name) => self.global_ref(&self.module.clone(), name),
            VarForeign(module, name) => {
                if self.arities.contains_key(&(module.clone(), name.clone())) {
                    self.global_ref(module, name)
                } else {
                    Expr::Foreign {
                        module: module.clone(),
                        name: name.clone(),
                    }
                }
            }
            VarCtor(home, _union, ctor) => self.ctor_value(home, ctor),
            List(items) => Expr::List(self.exprs(items)),
            Negate(inner) => Expr::Prim {
                op: PrimOp::Neg,
                args: vec![self.expr(inner, &TailCtx::No, false)],
            },
            Binop(op, home, function, left, right) => {
                self.binop(op, home, function, left, right)
            }
            Lambda(args, body) => {
                let name = self.fresh("lam");
                self.lift_with(name, args, body, None)
            }
            Call(func, args) => self.call(func, args, tail, is_tail),
            If(branches, otherwise) => Expr::If {
                branches: branches
                    .iter()
                    .map(|(condition, branch)| {
                        (
                            self.expr(condition, &TailCtx::No, false),
                            self.expr(branch, tail, is_tail),
                        )
                    })
                    .collect(),
                otherwise: Box::new(self.expr(otherwise, tail, is_tail)),
            },
            Let(decls, body) => self.let_expr(decls, body, tail, is_tail),
            Case(scrutinee, branches) => self.case(scrutinee, branches, tail, is_tail),
            Accessor(field) => self.accessor(field),
            Access(record, field) => Expr::Access {
                record: Box::new(self.expr(record, &TailCtx::No, false)),
                field: field.value.clone(),
            },
            Update(record, fields) => Expr::Update {
                record: Box::new(self.expr(record, &TailCtx::No, false)),
                fields: self.fields(fields),
            },
            Record(fields) => Expr::Record(self.fields(fields)),
            Tuple(a, b, rest) => {
                let mut items = vec![
                    self.expr(a, &TailCtx::No, false),
                    self.expr(b, &TailCtx::No, false),
                ];
                if let Some(c) = rest.first() {
                    items.push(self.expr(c, &TailCtx::No, false));
                }
                Expr::Tuple(items)
            }
        }
    }

    fn exprs(&mut self, exprs: &[can::Expr]) -> Vec<Expr> {
        exprs
            .iter()
            .map(|e| self.expr(e, &TailCtx::No, false))
            .collect()
    }

    fn fields(&mut self, fields: &[(crate::reporting::Located<Name>, can::Expr)]) -> Vec<(Name, Expr)> {
        fields
            .iter()
            .map(|(name, value)| (name.value.clone(), self.expr(value, &TailCtx::No, false)))
            .collect()
    }

    /// Reference a user-defined top-level: values directly, functions as
    /// capture-free closures.
    fn global_ref(&mut self, module: &Name, name: &Name) -> Expr {
        let global = global_name(module, name);
        let arity = self.arities[&(module.clone(), name.clone())];
        if arity == 0 {
            Expr::GlobalValue(global)
        } else {
            Expr::Closure {
                function: global,
                captures: Vec::new(),
            }
        }
    }

    /// A constructor used as a value.
    fn ctor_value(&mut self, home: &Name, ctor: &can::Ctor) -> Expr {
        match (home.as_str(), ctor.name.as_str()) {
            ("Basics", "True") => return Expr::Bool(true),
            ("Basics", "False") => return Expr::Bool(false),
            _ => {}
        }
        if ctor.arity == 0 {
            return Expr::Ctor {
                name: ctor.name.clone(),
                index: ctor.index,
                args: Vec::new(),
            };
        }
        let wrapper = self.ctor_wrapper(home, ctor);
        Expr::Closure {
            function: wrapper,
            captures: Vec::new(),
        }
    }

    /// Synthesize (once) a function that builds the constructor, so an
    /// unsaturated constructor is an ordinary closure.
    fn ctor_wrapper(&mut self, home: &Name, ctor: &can::Ctor) -> String {
        let name = format!("{}${}", mangle_module(home), ctor.name);
        if self.synthesized.insert(name.clone()) {
            let params: Vec<String> = (0..ctor.arity).map(|i| format!("a{}", i)).collect();
            self.functions.push(Function {
                name: name.clone(),
                params: params.clone(),
                captures: 0,
                tail_recursive: false,
                body: Expr::Ctor {
                    name: ctor.name.clone(),
                    index: ctor.index,
                    args: params.into_iter().map(Expr::Local).collect(),
                },
            });
        }
        name
    }

    /// Synthesize (once) `\r -> r.field` for a bare `.field` accessor.
    fn accessor(&mut self, field: &Name) -> Expr {
        let name = format!("$accessor${}", field);
        if self.synthesized.insert(name.clone()) {
            self.functions.push(Function {
                name: name.clone(),
                params: vec!["r".to_string()],
                captures: 0,
                tail_recursive: false,
                body: Expr::Access {
                    record: Box::new(Expr::Local("r".to_string())),
                    field: field.clone(),
                },
            });
        }
        Expr::Closure {
            function: name,
            captures: Vec::new(),
        }
    }

    // CALLS

    fn call(
        &mut self,
        func: &can::Expr,
        args: &[can::Expr],
        tail: &TailCtx,
        is_tail: bool,
    ) -> Expr {
        if is_tail {
            if let TailCtx::Loop { name, kind, arity } = tail {
                if is_self_ref(func, name, *kind) && args.len() == *arity {
                    return Expr::TailCall {
                        args: self.exprs(args),
                    };
                }
            }
        }

        use can::Expr_::*;
        match &func.value {
            VarCtor(home, _union, ctor) if args.len() == ctor.arity as usize => Expr::Ctor {
                name: ctor.name.clone(),
                index: ctor.index,
                args: self.exprs(args),
            },
            VarCtor(home, _union, ctor) => {
                // Under-applied constructor: apply through its wrapper.
                let wrapper = self.ctor_wrapper(home, ctor);
                Expr::CallClosure {
                    function: Box::new(Expr::Closure {
                        function: wrapper,
                        captures: Vec::new(),
                    }),
                    args: self.exprs(args),
                }
            }
            VarTopLevel(name) => {
                let module = self.module.clone();
                self.known_call(&module, name, args)
            }
            VarForeign(module, name)
                if self.arities.contains_key(&(module.clone(), name.clone())) =>
            {
                let module = module.clone();
                self.known_call(&module, name, args)
            }
            // A built-in kernel function called saturated: emit a direct
            // call to its exported symbol instead of the closure/apply path.
            VarForeign(module, name)
                if direct_builtin(module, name) == Some(args.len()) =>
            {
                Expr::CallBuiltin {
                    symbol: format!("rtb${}${}", module, name),
                    args: self.exprs(args),
                }
            }
            _ => Expr::CallClosure {
                function: Box::new(self.expr(func, &TailCtx::No, false)),
                args: self.exprs(args),
            },
        }
    }

    /// Call a user-defined top-level function: direct when saturated,
    /// through generic apply when partially or over-applied.
    fn known_call(&mut self, module: &Name, name: &Name, args: &[can::Expr]) -> Expr {
        let global = global_name(module, name);
        let arity = self.arities[&(module.clone(), name.clone())];
        let lowered = self.exprs(args);
        if arity == 0 {
            // A top-level value of function type.
            return Expr::CallClosure {
                function: Box::new(Expr::GlobalValue(global)),
                args: lowered,
            };
        }
        match lowered.len().cmp(&arity) {
            std::cmp::Ordering::Equal => Expr::CallDirect {
                function: global,
                args: lowered,
            },
            std::cmp::Ordering::Less => Expr::CallClosure {
                function: Box::new(Expr::Closure {
                    function: global,
                    captures: Vec::new(),
                }),
                args: lowered,
            },
            std::cmp::Ordering::Greater => {
                let (direct, rest) = lowered.split_at(arity);
                Expr::CallClosure {
                    function: Box::new(Expr::CallDirect {
                        function: global,
                        args: direct.to_vec(),
                    }),
                    args: rest.to_vec(),
                }
            }
        }
    }

    fn binop(
        &mut self,
        op: &Name,
        home: &Name,
        function: &Name,
        left: &can::Expr,
        right: &can::Expr,
    ) -> Expr {
        let prim = match op.as_str() {
            "+" => Some(PrimOp::Add),
            "-" => Some(PrimOp::Sub),
            "*" => Some(PrimOp::Mul),
            "/" => Some(PrimOp::FDiv),
            "//" => Some(PrimOp::IDiv),
            "^" => Some(PrimOp::Pow),
            "==" => Some(PrimOp::Eq),
            "/=" => Some(PrimOp::NotEq),
            "<" => Some(PrimOp::Lt),
            "<=" => Some(PrimOp::Le),
            ">" => Some(PrimOp::Gt),
            ">=" => Some(PrimOp::Ge),
            "&&" => Some(PrimOp::And),
            "||" => Some(PrimOp::Or),
            "++" => Some(PrimOp::Append),
            "::" => Some(PrimOp::Cons),
            _ => None,
        };
        if let Some(op) = prim {
            return Expr::Prim {
                op,
                args: vec![
                    self.expr(left, &TailCtx::No, false),
                    self.expr(right, &TailCtx::No, false),
                ],
            };
        }
        match op.as_str() {
            "|>" => self.call(right, std::slice::from_ref(left), &TailCtx::No, false),
            "<|" => self.call(left, std::slice::from_ref(right), &TailCtx::No, false),
            _ => {
                // A named operator function, e.g. `</>` from a package.
                if self
                    .arities
                    .contains_key(&(home.clone(), function.clone()))
                {
                    let home = home.clone();
                    let args = [left.clone(), right.clone()];
                    self.known_call(&home, function, &args)
                } else {
                    Expr::CallClosure {
                        function: Box::new(Expr::Foreign {
                            module: home.clone(),
                            name: function.clone(),
                        }),
                        args: vec![
                            self.expr(left, &TailCtx::No, false),
                            self.expr(right, &TailCtx::No, false),
                        ],
                    }
                }
            }
        }
    }

    // LET

    fn let_expr(
        &mut self,
        decls: &[can::LetDecl],
        body: &can::Expr,
        tail: &TailCtx,
        is_tail: bool,
    ) -> Expr {
        // Lower declarations first (allocation order), then fold them
        // around the lowered body back to front.
        enum Lowered {
            Single(String, Expr),
            Rec(Vec<(String, Expr)>),
        }

        let mut lowered_decls: Vec<Lowered> = Vec::new();
        for decl in decls {
            match decl {
                can::LetDecl::Def(def) => {
                    let value = self.local_def(def, false);
                    lowered_decls.push(Lowered::Single(def.name.value.to_string(), value));
                }
                can::LetDecl::Recursive(defs) => {
                    let bindings = defs
                        .iter()
                        .map(|def| (def.name.value.to_string(), self.local_def(def, true)))
                        .collect();
                    lowered_decls.push(Lowered::Rec(bindings));
                }
                can::LetDecl::Destruct(pattern, value) => {
                    let temp = self.fresh_temp();
                    let value = self.expr(value, &TailCtx::No, false);
                    lowered_decls.push(Lowered::Single(temp.clone(), value));
                    let mut tests = Vec::new();
                    let mut bindings = Vec::new();
                    self.pattern_tests(
                        pattern,
                        &Expr::Local(temp),
                        &mut tests,
                        &mut bindings,
                    );
                    for (name, path) in bindings {
                        lowered_decls.push(Lowered::Single(name, path));
                    }
                }
            }
        }

        let mut result = self.expr(body, tail, is_tail);
        for decl in lowered_decls.into_iter().rev() {
            result = match decl {
                Lowered::Single(name, value) => Expr::Let {
                    name,
                    value: Box::new(value),
                    body: Box::new(result),
                },
                Lowered::Rec(bindings) => Expr::LetRec {
                    bindings,
                    body: Box::new(result),
                },
            };
        }
        result
    }

    /// The value of a let-bound definition: functions are lifted into
    /// closures over their free variables, plain values lower directly.
    fn local_def(&mut self, def: &can::Def, recursive: bool) -> Expr {
        match def_function(def) {
            None => self.expr(&def.body, &TailCtx::No, false),
            Some((args, body)) => {
                let self_ref = recursive.then(|| (def.name.value.clone(), SelfKind::Local));
                self.lift_named(&def.name.value, args, body, self_ref)
            }
        }
    }

    fn lift_named(
        &mut self,
        def_name: &Name,
        args: &[can::Pattern],
        body: &can::Expr,
        self_ref: Option<(Name, SelfKind)>,
    ) -> Expr {
        self.counter += 1;
        let name = format!("{}${}{}", self.current_global, def_name, self.counter);
        self.lift_with(name, args, body, self_ref)
    }

    /// Closure-convert one function: compute its free variables, emit a
    /// lifted top-level function taking them as leading parameters, and
    /// return the closure allocation.
    fn lift_with(
        &mut self,
        name: String,
        args: &[can::Pattern],
        body: &can::Expr,
        self_ref: Option<(Name, SelfKind)>,
    ) -> Expr {
        let mut bound: Vec<Name> = Vec::new();
        for arg in args {
            pattern_vars(arg, &mut bound);
        }
        let mut free: BTreeSet<Name> = BTreeSet::new();
        free_vars(body, &mut bound, &mut free);
        let captures: Vec<Name> = free.into_iter().collect();

        self.function(name.clone(), captures.clone(), args, body, self_ref);

        Expr::Closure {
            function: name,
            captures: captures
                .into_iter()
                .map(|c| Expr::Local(c.to_string()))
                .collect(),
        }
    }

    // CASE

    fn case(
        &mut self,
        scrutinee: &can::Expr,
        branches: &[(can::Pattern, can::Expr)],
        tail: &TailCtx,
        is_tail: bool,
    ) -> Expr {
        let temp = self.fresh_temp();
        let scrutinee = self.expr(scrutinee, &TailCtx::No, false);
        let subject = Expr::Local(temp.clone());

        let mut lowered: Vec<Branch> = Vec::new();
        let mut exhaustive = false;
        for (pattern, body) in branches {
            let mut tests = Vec::new();
            let mut bindings = Vec::new();
            self.pattern_tests(pattern, &subject, &mut tests, &mut bindings);
            let body = self.expr(body, tail, is_tail);
            let catch_all = tests.is_empty();
            lowered.push(Branch {
                tests,
                bindings,
                body,
            });
            if catch_all {
                // Later branches are unreachable.
                exhaustive = true;
                break;
            }
        }
        if !exhaustive {
            lowered.push(Branch {
                tests: Vec::new(),
                bindings: Vec::new(),
                body: Expr::Crash(
                    "missing case branch (exhaustiveness checking should have caught this)"
                        .to_string(),
                ),
            });
        }

        Expr::Case {
            scrutinee: Box::new(scrutinee),
            temp,
            branches: lowered,
        }
    }

    /// The tests and bindings for matching `pattern` against `subject` —
    /// the IR twin of the JS backend's `pattern_tests`.
    fn pattern_tests(
        &mut self,
        pattern: &can::Pattern,
        subject: &Expr,
        tests: &mut Vec<(Expr, Test)>,
        bindings: &mut Vec<(String, Expr)>,
    ) {
        use can::Pattern_::*;
        let get = |of: &Expr, step: Step| Expr::GetField {
            of: Box::new(of.clone()),
            step,
        };
        match &pattern.value {
            Anything | Unit => {}
            Var(name) => bindings.push((name.to_string(), subject.clone())),
            Alias(inner, name) => {
                bindings.push((name.value.to_string(), subject.clone()));
                self.pattern_tests(inner, subject, tests, bindings);
            }
            Chr(c) => tests.push((subject.clone(), Test::IsChr(*c))),
            Str(s) => tests.push((subject.clone(), Test::IsStr(s.clone()))),
            Int(n) => tests.push((subject.clone(), Test::IsInt(*n))),
            Record(fields) => {
                for field in fields {
                    bindings.push((
                        field.value.to_string(),
                        Expr::Access {
                            record: Box::new(subject.clone()),
                            field: field.value.clone(),
                        },
                    ));
                }
            }
            Tuple(a, b, rest) => {
                self.pattern_tests(a, &get(subject, Step::TupleField(0)), tests, bindings);
                self.pattern_tests(b, &get(subject, Step::TupleField(1)), tests, bindings);
                if let Some(c) = rest.first() {
                    self.pattern_tests(c, &get(subject, Step::TupleField(2)), tests, bindings);
                }
            }
            Ctor(home, _union, ctor, args) => {
                match (home.as_str(), ctor.name.as_str()) {
                    ("Basics", "True") => tests.push((subject.clone(), Test::IsBool(true))),
                    ("Basics", "False") => tests.push((subject.clone(), Test::IsBool(false))),
                    _ => {
                        if ctor.num_ctors > 1 {
                            tests.push((
                                subject.clone(),
                                Test::IsCtor {
                                    name: ctor.name.clone(),
                                    index: ctor.index,
                                },
                            ));
                        }
                    }
                }
                for (i, arg) in args.iter().enumerate() {
                    self.pattern_tests(
                        arg,
                        &get(subject, Step::CtorArg(i as u32)),
                        tests,
                        bindings,
                    );
                }
            }
            List(items) => {
                let mut current = subject.clone();
                for item in items {
                    tests.push((current.clone(), Test::IsCons));
                    self.pattern_tests(item, &get(&current, Step::ListHead), tests, bindings);
                    current = get(&current, Step::ListTail);
                }
                tests.push((current, Test::IsNil));
            }
            Cons(head, tail) => {
                tests.push((subject.clone(), Test::IsCons));
                self.pattern_tests(head, &get(subject, Step::ListHead), tests, bindings);
                self.pattern_tests(tail, &get(subject, Step::ListTail), tests, bindings);
            }
        }
    }
}

// FREE VARIABLES — which locals a lambda body references from its
// enclosing scope; they become the lifted function's captures.

fn pattern_vars(pattern: &can::Pattern, out: &mut Vec<Name>) {
    use can::Pattern_::*;
    match &pattern.value {
        Anything | Unit | Chr(_) | Str(_) | Int(_) => {}
        Var(name) => out.push(name.clone()),
        Record(fields) => out.extend(fields.iter().map(|f| f.value.clone())),
        Alias(inner, name) => {
            out.push(name.value.clone());
            pattern_vars(inner, out);
        }
        Tuple(a, b, rest) => {
            pattern_vars(a, out);
            pattern_vars(b, out);
            for c in rest {
                pattern_vars(c, out);
            }
        }
        Ctor(_, _, _, args) => {
            for arg in args {
                pattern_vars(arg, out);
            }
        }
        List(items) => {
            for item in items {
                pattern_vars(item, out);
            }
        }
        Cons(head, tail) => {
            pattern_vars(head, out);
            pattern_vars(tail, out);
        }
    }
}

fn free_vars(expr: &can::Expr, bound: &mut Vec<Name>, out: &mut BTreeSet<Name>) {
    use can::Expr_::*;
    match &expr.value {
        VarLocal(name) => {
            if !bound.contains(name) {
                out.insert(name.clone());
            }
        }
        VarTopLevel(_) | VarForeign(..) | VarCtor(..) | Chr(_) | Str(_) | Int(_) | Float(_)
        | Accessor(_) | Unit | Shader(_) => {}
        List(items) => {
            for item in items {
                free_vars(item, bound, out);
            }
        }
        Negate(inner) => free_vars(inner, bound, out),
        Binop(_, _, _, left, right) => {
            free_vars(left, bound, out);
            free_vars(right, bound, out);
        }
        Lambda(args, body) => {
            let depth = bound.len();
            for arg in args {
                pattern_vars(arg, bound);
            }
            free_vars(body, bound, out);
            bound.truncate(depth);
        }
        Call(func, args) => {
            free_vars(func, bound, out);
            for arg in args {
                free_vars(arg, bound, out);
            }
        }
        If(branches, otherwise) => {
            for (condition, branch) in branches {
                free_vars(condition, bound, out);
                free_vars(branch, bound, out);
            }
            free_vars(otherwise, bound, out);
        }
        Let(decls, body) => {
            let depth = bound.len();
            // All let names are in scope everywhere in the let (they are
            // already dependency-sorted, so this cannot hide a real
            // reference to an outer binding — shadowing is illegal).
            for decl in decls {
                match decl {
                    can::LetDecl::Def(def) => bound.push(def.name.value.clone()),
                    can::LetDecl::Recursive(defs) => {
                        bound.extend(defs.iter().map(|d| d.name.value.clone()))
                    }
                    can::LetDecl::Destruct(pattern, _) => pattern_vars(pattern, bound),
                }
            }
            for decl in decls {
                match decl {
                    can::LetDecl::Def(def) => {
                        let inner_depth = bound.len();
                        for arg in &def.args {
                            pattern_vars(arg, bound);
                        }
                        free_vars(&def.body, bound, out);
                        bound.truncate(inner_depth);
                    }
                    can::LetDecl::Recursive(defs) => {
                        for def in defs {
                            let inner_depth = bound.len();
                            for arg in &def.args {
                                pattern_vars(arg, bound);
                            }
                            free_vars(&def.body, bound, out);
                            bound.truncate(inner_depth);
                        }
                    }
                    can::LetDecl::Destruct(_, value) => free_vars(value, bound, out),
                }
            }
            free_vars(body, bound, out);
            bound.truncate(depth);
        }
        Case(scrutinee, branches) => {
            free_vars(scrutinee, bound, out);
            for (pattern, branch) in branches {
                let depth = bound.len();
                pattern_vars(pattern, bound);
                free_vars(branch, bound, out);
                bound.truncate(depth);
            }
        }
        Access(record, _) => free_vars(record, bound, out),
        Update(record, fields) => {
            free_vars(record, bound, out);
            for (_, value) in fields {
                free_vars(value, bound, out);
            }
        }
        Record(fields) => {
            for (_, value) in fields {
                free_vars(value, bound, out);
            }
        }
        Tuple(a, b, rest) => {
            free_vars(a, bound, out);
            free_vars(b, bound, out);
            for c in rest {
                free_vars(c, bound, out);
            }
        }
    }
}

// TAIL CALLS — the IR twin of the JS backend's detection.

fn is_self_ref(expr: &can::Expr, name: &Name, kind: SelfKind) -> bool {
    match (&expr.value, kind) {
        (can::Expr_::VarTopLevel(n), SelfKind::TopLevel) => n == name,
        (can::Expr_::VarLocal(n), SelfKind::Local) => n == name,
        _ => false,
    }
}

fn has_self_tail_call(name: &Name, kind: SelfKind, arity: usize, body: &can::Expr) -> bool {
    use can::Expr_::*;
    match &body.value {
        Call(func, args) => is_self_ref(func, name, kind) && args.len() == arity,
        If(branches, otherwise) => {
            branches
                .iter()
                .any(|(_, b)| has_self_tail_call(name, kind, arity, b))
                || has_self_tail_call(name, kind, arity, otherwise)
        }
        Let(decls, inner) => {
            // A let definition of the same name would capture the name
            // (cannot happen — shadowing is illegal — but stay safe).
            let shadowed = decls.iter().any(|decl| match decl {
                can::LetDecl::Def(def) => def.name.value == *name,
                can::LetDecl::Recursive(defs) => defs.iter().any(|d| d.name.value == *name),
                can::LetDecl::Destruct(..) => false,
            });
            !shadowed && has_self_tail_call(name, kind, arity, inner)
        }
        Case(_, branches) => branches
            .iter()
            .any(|(_, b)| has_self_tail_call(name, kind, arity, b)),
        _ => false,
    }
}

/// Built-in kernel functions that have an exported `rtb$Module$name` symbol
/// (defined in `native_runtime.rs`) and can therefore be called directly,
/// bypassing the closure/apply machinery. Returns the arity; a call is only
/// lowered to `CallBuiltin` when it is saturated to exactly this arity.
///
/// This list must stay in sync with the `#[export_name = "rtb$…"]`
/// attributes in the native runtime.
fn direct_builtin(module: &Name, name: &Name) -> Option<usize> {
    Some(match (module.as_str(), name.as_str()) {
        ("Basics", "modBy") | ("Basics", "remainderBy") => 2,
        ("Basics", "clamp") => 3,
        ("List", "map") | ("List", "filter") | ("List", "member") | ("List", "range")
        | ("List", "repeat") | ("List", "indexedMap") => 2,
        ("List", "foldl") | ("List", "foldr") | ("List", "map2") => 3,
        ("List", "length") | ("List", "reverse") | ("List", "sum") | ("List", "product")
        | ("List", "head") | ("List", "tail") | ("List", "concat") | ("List", "isEmpty")
        | ("List", "filterMap") => 1,
        ("List", "take") | ("List", "drop") => 2,
        ("String", "fromInt") | ("String", "fromFloat") | ("String", "length") => 1,
        ("String", "join") => 2,
        _ => return None,
    })
}
