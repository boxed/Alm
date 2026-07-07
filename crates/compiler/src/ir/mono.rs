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
use crate::reporting::annotation::Located;
use crate::reporting::Region;

/// One concrete instance of a top-level function: the module it lives in, its
/// name, and the concrete type it is specialized at. Two uses of a polymorphic
/// function at the same concrete type share an instance.
#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub module: Name,
    pub name: Name,
    pub tipe: can::Type,
}

/// One module's checked data, as monomorphization consumes it.
pub struct ModuleInfo<'a> {
    pub name: Name,
    pub module: &'a can::Module,
    pub types: &'a HashMap<Name, can::Type>,
    pub node_types: &'a HashMap<Region, can::Type>,
}

/// A module's definitions plus the type info needed to specialize them.
struct ModuleCtx<'a> {
    defs: HashMap<Name, &'a can::Def>,
    types: &'a HashMap<Name, can::Type>,
    node_types: &'a HashMap<Region, can::Type>,
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
/// Convenience wrapper over [`analyze_project`].
pub fn analyze(
    module: &can::Module,
    types: &HashMap<Name, can::Type>,
    node_types: &HashMap<Region, can::Type>,
) -> MonoSet {
    let info = ModuleInfo {
        name: module.name.clone(),
        module,
        types,
        node_types,
    };
    analyze_project(std::slice::from_ref(&info), &module.name)
}

/// Compute the specializations reachable from the entry module's `main`
/// across the whole project. A reference to another project module's value
/// (`VarForeign` into a known module) seeds an instance in that module; a
/// reference to a built-in/kernel is recorded as a foreign use.
pub fn analyze_project(modules: &[ModuleInfo], entry: &Name) -> MonoSet {
    let ctxs = build_ctxs(modules);
    let mut set = MonoSet::default();
    // Seen instances keyed by (module, name, printed type).
    let mut seen: HashMap<(Name, Name, String), ()> = HashMap::new();
    let mut queue: Vec<Instance> = Vec::new();

    if let Some(entry_ctx) = ctxs.get(entry) {
        if let Some(main_ty) = entry_ctx.types.get(&Name::from("main")) {
            enqueue(
                &mut queue,
                &mut seen,
                &mut set,
                Instance {
                    module: entry.clone(),
                    name: Name::from("main"),
                    tipe: main_ty.clone(),
                },
            );
        }
    }

    while let Some(instance) = queue.pop() {
        let Some(mctx) = ctxs.get(&instance.module) else {
            continue;
        };
        let Some(def) = mctx.defs.get(&instance.name) else {
            continue; // referenced but not defined here (e.g. a port)
        };
        let scheme = mctx
            .types
            .get(&instance.name)
            .cloned()
            .unwrap_or_else(|| instance.tipe.clone());
        let mut subst = HashMap::new();
        match_type(&scheme, &instance.tipe, &mut subst);

        let mut refs = Vec::new();
        collect_refs(&def.body, &mut refs);
        for node in refs {
            let Some(captured) = mctx.node_types.get(&node.region) else {
                continue;
            };
            let concrete = apply_subst(&subst, captured);
            // Seed only concrete instances. A reference whose resolved type
            // still has a free variable is either genuinely ambiguous (e.g.
            // `UL.empty : UniqueList a`, harmless — no elements) or a reference
            // inside a local `let` body whose local type variables this
            // top-level substitution cannot resolve (e.g. a helper's `a` inside
            // `listShrinkRecurse`). Seeding the latter stamps out spurious
            // free-variable specializations with a boxed (`Ref`) layout that a
            // concrete call site would later read wrongly. Specialization itself
            // (the sink fixpoint, via `spec_let`) discovers every instance that
            // is actually reached, at its true concrete type — including the
            // genuinely-ambiguous ones — so skip free-variable seeds here.
            if type_has_free_tyvar(&concrete) {
                continue;
            }
            match &node.value {
                can::Expr_::VarTopLevel(name) if mctx.defs.contains_key(name) => {
                    enqueue(
                        &mut queue,
                        &mut seen,
                        &mut set,
                        Instance {
                            module: instance.module.clone(),
                            name: name.clone(),
                            tipe: concrete,
                        },
                    );
                }
                // A reference into another project module: specialize it there.
                can::Expr_::VarForeign(module, name) if ctxs.contains_key(module) => {
                    enqueue(
                        &mut queue,
                        &mut seen,
                        &mut set,
                        Instance {
                            module: module.clone(),
                            name: name.clone(),
                            tipe: concrete,
                        },
                    );
                }
                // A built-in/kernel: recorded for typed-kernel generation.
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

fn build_ctxs<'a>(modules: &'a [ModuleInfo<'a>]) -> HashMap<Name, ModuleCtx<'a>> {
    modules
        .iter()
        .map(|m| {
            (
                m.name.clone(),
                ModuleCtx {
                    defs: index_defs(m.module),
                    types: m.types,
                    node_types: m.node_types,
                },
            )
        })
        .collect()
}

fn enqueue(
    queue: &mut Vec<Instance>,
    seen: &mut HashMap<(Name, Name, String), ()>,
    set: &mut MonoSet,
    instance: Instance,
) {
    let key = (
        instance.module.clone(),
        instance.name.clone(),
        format!("{:?}", instance.tipe),
    );
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
        Chr(_) | Str(_) | Int(_) | Float(_) | Accessor(_) | Unit | Shader(_) => {}
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
        (Record(f1, ext1), Record(f2, _)) => {
            for (name, t1) in f1 {
                if let Some((_, t2)) = f2.iter().find(|(n, _)| n == name) {
                    match_type(t1, t2, subst);
                }
            }
            // A row variable (`{ r | field : t }`) stands for the concrete
            // record's fields not named explicitly here. Bind it so the open
            // record specializes to the full closed record — otherwise the
            // typed backend would lay it out with only the mentioned fields and
            // read subsequent fields at the wrong struct offsets.
            if let Some(ext) = ext1 {
                let named: std::collections::HashSet<&Name> =
                    f1.iter().map(|(n, _)| n).collect();
                let extra: Vec<(Name, can::Type)> = f2
                    .iter()
                    .filter(|(n, _)| !named.contains(n))
                    .cloned()
                    .collect();
                subst
                    .entry(ext.clone())
                    .or_insert_with(|| Record(extra, None));
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Specialized bodies
//
// Having discovered the instance set, `specialize_program` rebuilds each
// instance's body as a *typed* tree: every node carries its concrete type
// (the checker-captured type run through the instance's substitution) and
// every reference to another top-level function is resolved to that callee's
// mangled specialization name. This is the codegen-ready form; layouts and
// typed codegen consume it.
// ---------------------------------------------------------------------------

/// A fully monomorphized program: one typed function per specialization.
#[derive(Debug)]
pub struct MonoProgram {
    pub functions: Vec<TypedFn>,
}

/// A specialized function: its mangled name, the source name and concrete
/// type it came from, its typed parameters, and its typed body. `module` and
/// `region` locate the definition in its source `.elm` module for debug info.
#[derive(Debug, Clone)]
pub struct TypedFn {
    pub mangled: Name,
    pub original: Name,
    pub module: Name,
    pub tipe: can::Type,
    pub params: Vec<(can::Pattern, can::Type)>,
    pub body: TypedExpr,
    /// Source region of the definition (used for DWARF line info).
    pub region: Region,
}

/// An expression annotated with its concrete type and source region.
#[derive(Debug, Clone)]
pub struct TypedExpr {
    pub tipe: can::Type,
    pub kind: TypedKind,
    /// Source region this expression was built from. Synthetic (desugared)
    /// nodes inherit their enclosing expression's region.
    pub region: Region,
}

#[derive(Debug, Clone)]
pub enum TypedKind {
    Int(i64),
    Float(f64),
    Str(String),
    Chr(char),
    Unit,
    /// A locally-bound variable (lambda/let/case/argument).
    Local(Name),
    /// A resolved reference to another specialization (mangled name).
    Global(Name),
    /// A built-in/kernel value — still the uniform representation until
    /// typed kernels land.
    Foreign(Name, Name),
    Ctor(Name, Name, can::Ctor),
    List(Vec<TypedExpr>),
    Negate(Box<TypedExpr>),
    Binop(Name, Name, Name, Box<TypedExpr>, Box<TypedExpr>),
    Lambda(Vec<(can::Pattern, can::Type)>, Box<TypedExpr>),
    Call(Box<TypedExpr>, Vec<TypedExpr>),
    If(Vec<(TypedExpr, TypedExpr)>, Box<TypedExpr>),
    Let(Vec<TypedLetDecl>, Box<TypedExpr>),
    Case(Box<TypedExpr>, Vec<(can::Pattern, TypedExpr)>),
    Accessor(Name),
    Access(Box<TypedExpr>, Name),
    Update(Box<TypedExpr>, Vec<(Name, TypedExpr)>),
    Record(Vec<(Name, TypedExpr)>),
    Tuple(Box<TypedExpr>, Box<TypedExpr>, Option<Box<TypedExpr>>),
}

#[derive(Debug, Clone)]
pub enum TypedLetDecl {
    /// A local definition, specialized with the enclosing substitution.
    /// (Polymorphic-in-context local helpers are not yet split per type.)
    Def {
        name: Name,
        params: Vec<(can::Pattern, can::Type)>,
        body: TypedExpr,
    },
    Recursive(Vec<TypedLetDecl>),
    Destruct(can::Pattern, TypedExpr),
}

/// Analyze, then build a typed body for every discovered instance.
/// Analyze and specialize a single module. Convenience wrapper over
/// [`specialize_project`].
pub fn specialize_program(
    module: &can::Module,
    types: &HashMap<Name, can::Type>,
    node_types: &HashMap<Region, can::Type>,
) -> MonoProgram {
    let info = ModuleInfo {
        name: module.name.clone(),
        module,
        types,
        node_types,
    };
    specialize_project(std::slice::from_ref(&info), &module.name)
}

/// Analyze and specialize a whole project, emitting one typed function per
/// reachable specialization across all modules.
pub fn specialize_project(modules: &[ModuleInfo], entry: &Name) -> MonoProgram {
    let ctxs = build_ctxs(modules);

    let mut functions = Vec::new();
    // A worklist fixpoint: `analyze_project` seeds the reachable instances, but
    // specialization itself discovers more — every project reference resolves
    // to a concrete `Global` and is pushed onto `sink`, catching instances
    // (e.g. a foreign call inside a per-use-site-specialized local function)
    // whose concrete type only exists after specialization. Distinct instances
    // that share a mangled name (several `number` variables → `Int`, or
    // layout-identical `Ref` specializations) are compiled once.
    let sink: std::cell::RefCell<Vec<Instance>> = std::cell::RefCell::new(Vec::new());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut worklist: Vec<Instance> = Vec::new();
    for inst in analyze_project(modules, entry).instances {
        if seen.insert(mangle(&inst.module, &inst.name, &inst.tipe).to_string()) {
            worklist.push(inst);
        }
    }
    let mut wi = 0;
    while wi < worklist.len() {
        let instance = worklist[wi].clone();
        wi += 1;
        let Some(mctx) = ctxs.get(&instance.module) else {
            continue;
        };
        let Some(def) = mctx.defs.get(&instance.name) else {
            continue;
        };
        let mangled = mangle(&instance.module, &instance.name, &instance.tipe);
        let scheme = mctx
            .types
            .get(&instance.name)
            .cloned()
            .unwrap_or_else(|| instance.tipe.clone());
        let mut subst = HashMap::new();
        match_type(&scheme, &instance.tipe, &mut subst);

        // Peel argument types off the concrete function type.
        let mut params = Vec::new();
        let mut remaining = instance.tipe.clone();
        for arg in &def.args {
            match remaining {
                can::Type::Lambda(a, b) => {
                    params.push((arg.clone(), *a));
                    remaining = *b;
                }
                other => {
                    params.push((arg.clone(), other.clone()));
                    remaining = other;
                }
            }
        }

        let spec = Specializer {
            module: &instance.module,
            ctx: mctx,
            project: &ctxs,
            sink: &sink,
        };
        let mut body = spec.expr(&def.body, &subst);
        // Eta-normalize: when the declared parameters cover fewer arrows than
        // the concrete type -- the body itself has function type, as in
        // `mkAdder x y = (+) (x + y)` whose type is `Int -> Int -> Int -> Int`
        // -- append parameters and apply the body to them so the compiled arity
        // equals the type's arrow count. Partial application, point-free
        // wrapping, and closure application all assume that invariant (one
        // parameter per arrow); a shortfall makes them read past the closure's
        // arguments and return garbage.
        let mut idx = 0u32;
        while let can::Type::Lambda(a, b) = remaining {
            let pname = Name::from(format!(
                "_etf_{}_{}_{}",
                def.name.region.start.row, def.name.region.start.col, idx
            ));
            let pat = Located::new(def.name.region, can::Pattern_::Var(pname.clone()));
            params.push((pat, (*a).clone()));
            body = TypedExpr {
                tipe: (*b).clone(),
                kind: TypedKind::Call(
                    Box::new(body),
                    vec![TypedExpr {
                        tipe: (*a).clone(),
                        kind: TypedKind::Local(pname),
                        region: def.name.region,
                    }],
                ),
                region: def.name.region,
            };
            remaining = *b;
            idx += 1;
        }
        functions.push(TypedFn {
            mangled,
            original: instance.name.clone(),
            module: instance.module.clone(),
            tipe: instance.tipe.clone(),
            params,
            body,
            region: def.name.region,
        });
        // Enqueue every instance this body's references demanded.
        for inst in sink.borrow_mut().drain(..) {
            if seen.insert(mangle(&inst.module, &inst.name, &inst.tipe).to_string()) {
                worklist.push(inst);
            }
        }
    }

    MonoProgram { functions }
}

struct Specializer<'a> {
    /// The module whose definition is being specialized (for resolving local
    /// top-level references).
    module: &'a Name,
    ctx: &'a ModuleCtx<'a>,
    project: &'a HashMap<Name, ModuleCtx<'a>>,
    /// Every project-level reference this specialization resolves to a concrete
    /// `Global`, recorded as an instance to compile. This drives discovery from
    /// specialization itself, so references that only become concrete after
    /// per-use-site local monomorphization (`spec_let`) are still found — a
    /// plain AST walk under the top-level substitution would leave their types
    /// polymorphic and miss the concrete instantiation.
    sink: &'a std::cell::RefCell<Vec<Instance>>,
}

impl Specializer<'_> {
    /// The concrete type of a node under a substitution.
    fn node_ty(&self, expr: &can::Expr, subst: &HashMap<Name, can::Type>) -> can::Type {
        let captured = self
            .ctx
            .node_types
            .get(&expr.region)
            .cloned()
            .unwrap_or(can::Type::Unit);
        apply_subst(subst, &captured)
    }

    fn expr(&self, expr: &can::Expr, subst: &HashMap<Name, can::Type>) -> TypedExpr {
        use can::Expr_::*;
        let tipe = self.node_ty(expr, subst);
        let kind = match &expr.value {
            Int(n) => TypedKind::Int(*n),
            Float(f) => TypedKind::Float(*f),
            Str(s) => TypedKind::Str(s.clone()),
            Chr(c) => TypedKind::Chr(*c),
            Unit => TypedKind::Unit,
            // WebGL shaders are JS-backend only; the native/typed path never
            // renders them. Carry the source so the node is well-formed.
            Shader(shader) => TypedKind::Str(shader.src.clone()),
            VarLocal(name) => TypedKind::Local(name.clone()),
            VarTopLevel(name) => {
                if self.ctx.defs.contains_key(name) {
                    // Resolve to the callee's specialization (this module) at
                    // this node's concrete type.
                    self.sink.borrow_mut().push(Instance {
                        module: self.module.clone(),
                        name: name.clone(),
                        tipe: tipe.clone(),
                    });
                    TypedKind::Global(mangle(self.module, name, &tipe))
                } else {
                    TypedKind::Local(name.clone())
                }
            }
            VarForeign(module, name) => {
                // A value from another project module resolves to its
                // specialization; a built-in stays a kernel reference.
                if self.project.contains_key(module) {
                    self.sink.borrow_mut().push(Instance {
                        module: module.clone(),
                        name: name.clone(),
                        tipe: tipe.clone(),
                    });
                    TypedKind::Global(mangle(module, name, &tipe))
                } else {
                    TypedKind::Foreign(module.clone(), name.clone())
                }
            }
            VarCtor(home, union, ctor) => {
                TypedKind::Ctor(home.clone(), union.clone(), ctor.clone())
            }
            List(items) => {
                TypedKind::List(items.iter().map(|e| self.expr(e, subst)).collect())
            }
            Negate(inner) => TypedKind::Negate(Box::new(self.expr(inner, subst))),
            Binop(op, home, func, l, r) => {
                let lt = self.expr(l, subst);
                let rt = self.expr(r, subst);
                if is_native_binop(op.as_str()) {
                    TypedKind::Binop(
                        op.clone(),
                        home.clone(),
                        func.clone(),
                        Box::new(lt),
                        Box::new(rt),
                    )
                } else {
                    // A custom operator (e.g. elm/parser's `|=`, `|.`) is sugar
                    // for applying its resolving function to both operands. The
                    // typed backend's `gen_binop` only knows the numeric and
                    // comparison built-ins, so lower everything else to a plain
                    // call — which also lets the specializer discover the
                    // operator's function like any other reference.
                    let func_ty = can::Type::Lambda(
                        Box::new(lt.tipe.clone()),
                        Box::new(can::Type::Lambda(
                            Box::new(rt.tipe.clone()),
                            Box::new(tipe.clone()),
                        )),
                    );
                    let callee_kind = if self.project.contains_key(home) {
                        self.sink.borrow_mut().push(Instance {
                            module: home.clone(),
                            name: func.clone(),
                            tipe: func_ty.clone(),
                        });
                        TypedKind::Global(mangle(home, func, &func_ty))
                    } else {
                        TypedKind::Foreign(home.clone(), func.clone())
                    };
                    let callee = TypedExpr {
                        tipe: func_ty,
                        kind: callee_kind,
                        region: expr.region,
                    };
                    TypedKind::Call(Box::new(callee), vec![lt, rt])
                }
            }
            Lambda(args, body) => {
                // The lambda node's type is arg1 -> .. -> argN -> body; peel
                // it to type each parameter.
                let mut params = Vec::new();
                let mut remaining = tipe.clone();
                for arg in args {
                    match remaining {
                        can::Type::Lambda(a, b) => {
                            params.push((arg.clone(), *a));
                            remaining = *b;
                        }
                        other => {
                            params.push((arg.clone(), other.clone()));
                            remaining = other;
                        }
                    }
                }
                let mut body_expr = self.expr(body, subst);
                // A closure's compiled arity must equal its type's total arrow
                // count: whoever applies it (see `apply_closure_curried`) passes
                // one argument per arrow, so a closure carrying fewer parameters
                // than arrows would read past its arguments and return garbage.
                //
                // (1) Flatten directly-nested lambdas -- `\a -> \b -> e` becomes
                //     `\a b -> e` -- exactly as Elm collapses curried lambdas.
                //     Only syntactic nesting is merged; a `let`/`if` between the
                //     arrows leaves the inner lambda in place, preserving when
                //     its body runs.
                while let TypedKind::Lambda(inner_params, inner_body) = body_expr.kind {
                    params.extend(inner_params);
                    body_expr = *inner_body;
                }
                // (2) Eta-expand any arrows the parameters do not yet cover --
                //     point-free tails such as `\_ -> identity` -- by applying
                //     the body to fresh parameters. Pure, so semantics-preserving.
                let mut covered = tipe.clone();
                for _ in 0..params.len() {
                    match covered {
                        can::Type::Lambda(_, b) => covered = *b,
                        other => {
                            covered = other;
                            break;
                        }
                    }
                }
                let mut eta_args: Vec<TypedExpr> = Vec::new();
                let mut idx = 0u32;
                while let can::Type::Lambda(a, b) = covered {
                    let pname = Name::from(format!(
                        "_eta{}_{}_{}",
                        expr.region.start.row, expr.region.start.col, idx
                    ));
                    let pat = Located::new(
                        expr.region,
                        can::Pattern_::Var(pname.clone()),
                    );
                    params.push((pat, (*a).clone()));
                    eta_args.push(TypedExpr {
                        tipe: (*a).clone(),
                        kind: TypedKind::Local(pname),
                        region: expr.region,
                    });
                    covered = *b;
                    idx += 1;
                }
                if !eta_args.is_empty() {
                    body_expr = TypedExpr {
                        tipe: covered,
                        kind: TypedKind::Call(Box::new(body_expr), eta_args),
                        region: expr.region,
                    };
                }
                TypedKind::Lambda(params, Box::new(body_expr))
            }
            Call(func, args) => TypedKind::Call(
                Box::new(self.expr(func, subst)),
                args.iter().map(|e| self.expr(e, subst)).collect(),
            ),
            If(branches, otherwise) => TypedKind::If(
                branches
                    .iter()
                    .map(|(c, b)| (self.expr(c, subst), self.expr(b, subst)))
                    .collect(),
                Box::new(self.expr(otherwise, subst)),
            ),
            Let(decls, body) => self.spec_let(decls, body, subst),
            Case(scrutinee, branches) => TypedKind::Case(
                Box::new(self.expr(scrutinee, subst)),
                branches
                    .iter()
                    .map(|(p, b)| (p.clone(), self.expr(b, subst)))
                    .collect(),
            ),
            Accessor(field) => TypedKind::Accessor(field.clone()),
            Access(record, field) => {
                TypedKind::Access(Box::new(self.expr(record, subst)), field.value.clone())
            }
            Update(record, fields) => TypedKind::Update(
                Box::new(self.expr(record, subst)),
                fields
                    .iter()
                    .map(|(n, v)| (n.value.clone(), self.expr(v, subst)))
                    .collect(),
            ),
            Record(fields) => TypedKind::Record(
                fields
                    .iter()
                    .map(|(n, v)| (n.value.clone(), self.expr(v, subst)))
                    .collect(),
            ),
            Tuple(a, b, rest) => TypedKind::Tuple(
                Box::new(self.expr(a, subst)),
                Box::new(self.expr(b, subst)),
                rest.first().map(|c| Box::new(self.expr(c, subst))),
            ),
        };
        TypedExpr {
            tipe,
            kind,
            region: expr.region,
        }
    }

    fn let_decl(
        &self,
        decl: &can::LetDecl,
        subst: &HashMap<Name, can::Type>,
    ) -> TypedLetDecl {
        match decl {
            can::LetDecl::Def(def) => {
                let (params, body) = self.local_def_params_body(def, subst);
                TypedLetDecl::Def {
                    name: def.name.value.clone(),
                    params,
                    body,
                }
            }
            can::LetDecl::Recursive(defs) => TypedLetDecl::Recursive(
                defs.iter()
                    .map(|def| {
                        let (params, body) = self.local_def_params_body(def, subst);
                        TypedLetDecl::Def {
                            name: def.name.value.clone(),
                            params,
                            body,
                        }
                    })
                    .collect(),
            ),
            can::LetDecl::Destruct(pattern, value) => {
                TypedLetDecl::Destruct(pattern.clone(), self.expr(value, subst))
            }
        }
    }

    /// Specialize a `let` block.
    ///
    /// A local `let` function that is *polymorphic in the enclosing context*
    /// (its recorded type still has a free type variable after the enclosing
    /// substitution) cannot be laid out concretely from a single copy: its
    /// parameters would fall back to the boxed `Ref` layout while call sites
    /// pass concrete unboxed values. So such a function is specialized *per
    /// use-site type*, exactly as top-level functions are — one copy per
    /// distinct concrete instantiation, each with concrete parameter/return
    /// layouts — and every reference is routed to the matching copy by name.
    ///
    /// Monomorphic locals (and any local whose type is already concrete here)
    /// keep the ordinary single-copy behaviour.
    fn spec_let(
        &self,
        decls: &[can::LetDecl],
        body: &can::Expr,
        subst: &HashMap<Name, can::Type>,
    ) -> TypedKind {
        // Identify the polymorphic-in-context local *functions* in this block:
        // name -> (definition, its generic recorded type, whether recursive).
        struct PolyLocal<'a> {
            def: &'a can::Def,
            generic: can::Type,
            recursive: bool,
        }
        let mut polys: HashMap<Name, PolyLocal> = HashMap::new();
        let mut poly_index: HashMap<usize, Name> = HashMap::new();
        for (i, decl) in decls.iter().enumerate() {
            let (def, recursive) = match decl {
                // Any local binding whose type is polymorphic in context needs
                // per-use-site specialization -- including a *point-free* one
                // (zero args), e.g. `f = List.sort << g`. Compiling such a
                // binding once under the enclosing substitution leaves its type
                // variable free, producing a boxed (`Ref`) specialization of the
                // functions it composes; a concrete unboxed call site then reads
                // those values at the wrong layout. The `type_has_free_tyvar`
                // check below filters out monomorphic values (`x = 5`).
                can::LetDecl::Def(def) => (def, false),
                can::LetDecl::Recursive(defs) if defs.len() == 1 && !defs[0].args.is_empty() => {
                    (&defs[0], true)
                }
                _ => continue,
            };
            let Some(generic) = self.ctx.node_types.get(&def.name.region) else {
                continue;
            };
            if type_has_free_tyvar(&apply_subst(subst, generic)) {
                poly_index.insert(i, def.name.value.clone());
                polys.insert(
                    def.name.value.clone(),
                    PolyLocal {
                        def,
                        generic: generic.clone(),
                        recursive,
                    },
                );
            }
        }

        // No polymorphic locals: ordinary specialization.
        if polys.is_empty() {
            return TypedKind::Let(
                decls.iter().map(|d| self.let_decl(d, subst)).collect(),
                Box::new(self.expr(body, subst)),
            );
        }

        let poly_names: std::collections::HashSet<Name> = polys.keys().cloned().collect();

        // Produce the non-poly decls (in place, keyed by index) and the body.
        let mut nonpoly: HashMap<usize, TypedLetDecl> = HashMap::new();
        for (i, decl) in decls.iter().enumerate() {
            if poly_index.contains_key(&i) {
                continue;
            }
            nonpoly.insert(i, self.let_decl(decl, subst));
        }
        let mut body_t = self.expr(body, subst);

        // Fixpoint: discover every concrete instantiation of each poly local,
        // building one specialized copy per distinct type. Seed from the body
        // and the non-poly decls; each specialized body is scanned in turn so
        // recursion and poly-to-poly calls are covered.
        let mut queue: Vec<(Name, can::Type)> = Vec::new();
        {
            let mut masked = std::collections::HashSet::new();
            scan_local_uses(&body_t, &poly_names, &mut masked, &mut queue);
            for d in nonpoly.values() {
                scan_decl_uses(d, &poly_names, &mut queue);
            }
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut spec_decls: HashMap<String, TypedLetDecl> = HashMap::new();
        let mut per_name: HashMap<Name, Vec<Name>> = HashMap::new();
        let mut qi = 0;
        while qi < queue.len() {
            let (name, ty) = queue[qi].clone();
            qi += 1;
            let mangled = mangle_local(&name, &ty);
            if !seen.insert(mangled.to_string()) {
                continue;
            }
            let poly = &polys[&name];
            let g_sub = apply_subst(subst, &poly.generic);
            let mut subst_i = subst.clone();
            match_type(&g_sub, &ty, &mut subst_i);
            let (params, spec_body) = self.local_def_params_body(poly.def, &subst_i);
            {
                let mut masked = std::collections::HashSet::new();
                scan_local_uses(&spec_body, &poly_names, &mut masked, &mut queue);
            }
            let inner = TypedLetDecl::Def {
                name: mangled.clone(),
                params,
                body: spec_body,
            };
            let decl = if poly.recursive {
                TypedLetDecl::Recursive(vec![inner])
            } else {
                inner
            };
            spec_decls.insert(mangled.to_string(), decl);
            per_name.entry(name).or_default().push(mangled);
        }

        // Assemble the output decls, preserving the original order: each poly
        // local expands in place into its specialized copies; a poly local with
        // no discovered use falls back to the ordinary single copy.
        let mut out_decls: Vec<TypedLetDecl> = Vec::new();
        let mut specialized: std::collections::HashSet<Name> = std::collections::HashSet::new();
        for (i, decl) in decls.iter().enumerate() {
            match poly_index.get(&i) {
                Some(name) => match per_name.get(name) {
                    Some(mangleds) if !mangleds.is_empty() => {
                        specialized.insert(name.clone());
                        for m in mangleds {
                            out_decls.push(spec_decls.remove(m.as_str()).unwrap());
                        }
                    }
                    _ => out_decls.push(self.let_decl(decl, subst)),
                },
                None => out_decls.push(nonpoly.remove(&i).unwrap()),
            }
        }

        // Route every use of a specialized local to the copy for its concrete
        // type. `emitted` guards against rewriting to a copy that (defensively)
        // does not exist.
        let emitted: std::collections::HashSet<String> = seen;
        {
            let mut masked = std::collections::HashSet::new();
            rewrite_local_uses(&mut body_t, &specialized, &emitted, &mut masked);
        }
        for d in out_decls.iter_mut() {
            rewrite_decl_uses(d, &specialized, &emitted);
        }

        TypedKind::Let(out_decls, Box::new(body_t))
    }

    /// Peel a `let`-bound definition's parameter types off its recorded
    /// function type (captured at `def.name.region` by the checker), applying
    /// the enclosing substitution. Falls back to `Unit` when the type is
    /// unavailable or under-applied (a non-function value binding).
    /// Peel a local definition's declared parameters off its (substituted)
    /// recorded function type and eta-normalize the shortfall.
    ///
    /// If the type has more arrows than the definition has parameters -- a
    /// point-free local such as `insertOrFail newName = Result.andThen (...)`
    /// whose type is `String -> Result -> Result` -- append one synthetic
    /// parameter per uncovered arrow and apply the body to them, so the
    /// compiled arity equals the type's arrow count. This mirrors the
    /// top-level eta-normalization in `specialize_project`. Higher-order
    /// kernels (`List.foldl`, `List.concatMap`, ...) and closure application
    /// pass one argument per arrow; a compiled shortfall would apply the local
    /// with more arguments than it has parameters and read past the closure
    /// (previously a memory-corrupting arity mismatch).
    fn local_def_params_body(
        &self,
        def: &can::Def,
        subst: &HashMap<Name, can::Type>,
    ) -> (Vec<(can::Pattern, can::Type)>, TypedExpr) {
        let mut remaining = self
            .ctx
            .node_types
            .get(&def.name.region)
            .map(|t| apply_subst(subst, t));
        let mut params: Vec<(can::Pattern, can::Type)> = Vec::new();
        for arg in &def.args {
            let ty = match remaining.take() {
                Some(can::Type::Lambda(a, b)) => {
                    remaining = Some(*b);
                    *a
                }
                other => {
                    remaining = other;
                    can::Type::Unit
                }
            };
            params.push((arg.clone(), ty));
        }
        let mut body = self.expr(&def.body, subst);
        let mut idx = 0u32;
        while let Some(can::Type::Lambda(a, b)) = remaining {
            let pname = Name::from(format!(
                "_etl_{}_{}_{}",
                def.name.region.start.row, def.name.region.start.col, idx
            ));
            let pat = Located::new(def.name.region, can::Pattern_::Var(pname.clone()));
            params.push((pat, (*a).clone()));
            body = TypedExpr {
                tipe: (*b).clone(),
                kind: TypedKind::Call(
                    Box::new(body),
                    vec![TypedExpr {
                        tipe: (*a).clone(),
                        kind: TypedKind::Local(pname),
                        region: def.name.region,
                    }],
                ),
                region: def.name.region,
            };
            remaining = Some(*b);
            idx += 1;
        }
        (params, body)
    }
}

/// Whether a type still carries a free type variable that would drive a boxed
/// (`Ref`) layout. A `number` variable is excluded: it defaults to `Int`
/// consistently, so it needs no per-use specialization.
fn type_has_free_tyvar(tipe: &can::Type) -> bool {
    use can::Type::*;
    match tipe {
        Var(n) => !n.as_str().starts_with("number"),
        Lambda(a, b) => type_has_free_tyvar(a) || type_has_free_tyvar(b),
        Type(_, _, args) => args.iter().any(type_has_free_tyvar),
        Tuple(a, b, c) => {
            type_has_free_tyvar(a)
                || type_has_free_tyvar(b)
                || c.as_ref().map_or(false, |c| type_has_free_tyvar(c))
        }
        // An open record `{ a | field : T, .. }` carries a free row variable
        // (the extension `a`). It is genuinely polymorphic: its concrete field
        // set — and therefore the sorted field offsets a record access compiles
        // to — depends on what `a` resolves to at each use site. A local
        // function with such a parameter must be specialized per use-site (like
        // any polymorphic local); otherwise its accesses read fields at the
        // offsets of the partial (open) field set instead of the concrete
        // record's, returning the wrong field.
        Record(fields, ext) => ext.is_some() || fields.iter().any(|(_, t)| type_has_free_tyvar(t)),
        Unit => false,
    }
}

/// Operators the typed backend generates inline (pipes, composition, cons,
/// append, boolean, equality, and the numeric/ordering built-ins). Any other
/// operator is a library-defined function and is lowered to a plain call.
fn is_native_binop(op: &str) -> bool {
    matches!(
        op,
        "|>" | "<|"
            | "<<"
            | ">>"
            | "::"
            | "++"
            | "&&"
            | "||"
            | "=="
            | "/="
            | "+"
            | "-"
            | "*"
            | "/"
            | "//"
            | "^"
            | "<"
            | "<="
            | ">"
            | ">="
    )
}

/// The mangled name of a local specialization: the source name plus its
/// concrete type, mirroring [`mangle`] for top-level functions.
fn mangle_local(name: &Name, tipe: &can::Type) -> Name {
    Name::from(format!("{}${}", name, mangle_type(tipe)))
}

/// The names a pattern binds (used to detect shadowing of a specialized local).
fn pattern_bound_names(pattern: &can::Pattern, out: &mut Vec<Name>) {
    use can::Pattern_::*;
    match &pattern.value {
        Var(name) => out.push(name.clone()),
        Alias(inner, name) => {
            pattern_bound_names(inner, out);
            out.push(name.value.clone());
        }
        Record(fields) => out.extend(fields.iter().map(|f| f.value.clone())),
        Tuple(a, b, rest) => {
            pattern_bound_names(a, out);
            pattern_bound_names(b, out);
            rest.iter().for_each(|p| pattern_bound_names(p, out));
        }
        Ctor(_, _, _, args) => args.iter().for_each(|p| pattern_bound_names(p, out)),
        List(items) => items.iter().for_each(|p| pattern_bound_names(p, out)),
        Cons(h, t) => {
            pattern_bound_names(h, out);
            pattern_bound_names(t, out);
        }
        Anything | Unit | Chr(_) | Str(_) | Int(_) => {}
    }
}

/// The names a `let` block's declarations bind at block scope (def names and
/// destructure-pattern names) — the set an inner block shadows.
fn decl_bound_names(decls: &[TypedLetDecl], out: &mut Vec<Name>) {
    for decl in decls {
        match decl {
            TypedLetDecl::Def { name, .. } => out.push(name.clone()),
            TypedLetDecl::Recursive(defs) => decl_bound_names(defs, out),
            TypedLetDecl::Destruct(pattern, _) => pattern_bound_names(pattern, out),
        }
    }
}

/// Run `f` with the poly-local names bound by `binders` masked (shadowed) in
/// `masked`, restoring the mask afterwards.
fn with_masked<R>(
    binders: &[Name],
    poly: &std::collections::HashSet<Name>,
    masked: &mut std::collections::HashSet<Name>,
    f: impl FnOnce(&mut std::collections::HashSet<Name>) -> R,
) -> R {
    let added: Vec<Name> = binders
        .iter()
        .filter(|n| poly.contains(*n) && masked.insert((*n).clone()))
        .cloned()
        .collect();
    let r = f(masked);
    for n in added {
        masked.remove(&n);
    }
    r
}

/// Collect uses of any poly-local `name` in a typed expression as
/// `(name, concrete type at the use)`, skipping references shadowed by an
/// inner binder that rebinds the name.
fn scan_local_uses(
    e: &TypedExpr,
    poly: &std::collections::HashSet<Name>,
    masked: &mut std::collections::HashSet<Name>,
    out: &mut Vec<(Name, can::Type)>,
) {
    use TypedKind::*;
    match &e.kind {
        Local(n) => {
            if poly.contains(n) && !masked.contains(n) {
                out.push((n.clone(), e.tipe.clone()));
            }
        }
        Int(_) | Float(_) | Str(_) | Chr(_) | Unit | Global(_) | Foreign(..) | Ctor(..)
        | Accessor(_) => {}
        Negate(x) | Access(x, _) => scan_local_uses(x, poly, masked, out),
        List(xs) => xs.iter().for_each(|x| scan_local_uses(x, poly, masked, out)),
        Binop(_, _, _, l, r) => {
            scan_local_uses(l, poly, masked, out);
            scan_local_uses(r, poly, masked, out);
        }
        Call(f, args) => {
            scan_local_uses(f, poly, masked, out);
            args.iter().for_each(|a| scan_local_uses(a, poly, masked, out));
        }
        If(branches, otherwise) => {
            for (c, b) in branches {
                scan_local_uses(c, poly, masked, out);
                scan_local_uses(b, poly, masked, out);
            }
            scan_local_uses(otherwise, poly, masked, out);
        }
        Update(r, fields) => {
            scan_local_uses(r, poly, masked, out);
            fields.iter().for_each(|(_, v)| scan_local_uses(v, poly, masked, out));
        }
        Record(fields) => fields.iter().for_each(|(_, v)| scan_local_uses(v, poly, masked, out)),
        Tuple(a, b, c) => {
            scan_local_uses(a, poly, masked, out);
            scan_local_uses(b, poly, masked, out);
            if let Some(c) = c {
                scan_local_uses(c, poly, masked, out);
            }
        }
        Lambda(params, body) => {
            let mut binders = Vec::new();
            for (p, _) in params {
                pattern_bound_names(p, &mut binders);
            }
            with_masked(&binders, poly, masked, |m| scan_local_uses(body, poly, m, out));
        }
        Case(scrut, branches) => {
            scan_local_uses(scrut, poly, masked, out);
            for (p, b) in branches {
                let mut binders = Vec::new();
                pattern_bound_names(p, &mut binders);
                with_masked(&binders, poly, masked, |m| scan_local_uses(b, poly, m, out));
            }
        }
        Let(decls, body) => {
            let mut binders = Vec::new();
            decl_bound_names(decls, &mut binders);
            with_masked(&binders, poly, masked, |m| {
                for d in decls {
                    scan_decl_uses_masked(d, poly, m, out);
                }
                scan_local_uses(body, poly, m, out);
            });
        }
    }
}

/// Scan a decl's sub-expressions for poly-local uses (top-level entry; the
/// block's own binders are assumed already masked by the caller).
fn scan_decl_uses(
    decl: &TypedLetDecl,
    poly: &std::collections::HashSet<Name>,
    out: &mut Vec<(Name, can::Type)>,
) {
    let mut masked = std::collections::HashSet::new();
    scan_decl_uses_masked(decl, poly, &mut masked, out);
}

fn scan_decl_uses_masked(
    decl: &TypedLetDecl,
    poly: &std::collections::HashSet<Name>,
    masked: &mut std::collections::HashSet<Name>,
    out: &mut Vec<(Name, can::Type)>,
) {
    match decl {
        TypedLetDecl::Def { params, body, .. } => {
            let mut binders = Vec::new();
            for (p, _) in params {
                pattern_bound_names(p, &mut binders);
            }
            with_masked(&binders, poly, masked, |m| scan_local_uses(body, poly, m, out));
        }
        TypedLetDecl::Recursive(defs) => {
            for d in defs {
                scan_decl_uses_masked(d, poly, masked, out);
            }
        }
        TypedLetDecl::Destruct(_, value) => scan_local_uses(value, poly, masked, out),
    }
}

/// Rewrite every use of a specialized local to the mangled copy for the use's
/// concrete type, respecting shadowing. `emitted` is the set of copies that
/// actually exist; a use whose copy is missing is left untouched.
fn rewrite_local_uses(
    e: &mut TypedExpr,
    specialized: &std::collections::HashSet<Name>,
    emitted: &std::collections::HashSet<String>,
    masked: &mut std::collections::HashSet<Name>,
) {
    use TypedKind::*;
    let tipe = e.tipe.clone();
    match &mut e.kind {
        Local(n) => {
            if specialized.contains(n) && !masked.contains(n) {
                let m = mangle_local(n, &tipe);
                if emitted.contains(m.as_str()) {
                    *n = m;
                }
            }
        }
        Int(_) | Float(_) | Str(_) | Chr(_) | Unit | Global(_) | Foreign(..) | Ctor(..)
        | Accessor(_) => {}
        Negate(x) | Access(x, _) => rewrite_local_uses(x, specialized, emitted, masked),
        List(xs) => xs
            .iter_mut()
            .for_each(|x| rewrite_local_uses(x, specialized, emitted, masked)),
        Binop(_, _, _, l, r) => {
            rewrite_local_uses(l, specialized, emitted, masked);
            rewrite_local_uses(r, specialized, emitted, masked);
        }
        Call(f, args) => {
            rewrite_local_uses(f, specialized, emitted, masked);
            args.iter_mut()
                .for_each(|a| rewrite_local_uses(a, specialized, emitted, masked));
        }
        If(branches, otherwise) => {
            for (c, b) in branches.iter_mut() {
                rewrite_local_uses(c, specialized, emitted, masked);
                rewrite_local_uses(b, specialized, emitted, masked);
            }
            rewrite_local_uses(otherwise, specialized, emitted, masked);
        }
        Update(r, fields) => {
            rewrite_local_uses(r, specialized, emitted, masked);
            fields
                .iter_mut()
                .for_each(|(_, v)| rewrite_local_uses(v, specialized, emitted, masked));
        }
        Record(fields) => fields
            .iter_mut()
            .for_each(|(_, v)| rewrite_local_uses(v, specialized, emitted, masked)),
        Tuple(a, b, c) => {
            rewrite_local_uses(a, specialized, emitted, masked);
            rewrite_local_uses(b, specialized, emitted, masked);
            if let Some(c) = c {
                rewrite_local_uses(c, specialized, emitted, masked);
            }
        }
        Lambda(params, body) => {
            let mut binders = Vec::new();
            for (p, _) in params.iter() {
                pattern_bound_names(p, &mut binders);
            }
            with_masked(&binders, specialized, masked, |m| {
                rewrite_local_uses(body, specialized, emitted, m)
            });
        }
        Case(scrut, branches) => {
            rewrite_local_uses(scrut, specialized, emitted, masked);
            for (p, b) in branches.iter_mut() {
                let mut binders = Vec::new();
                pattern_bound_names(p, &mut binders);
                with_masked(&binders, specialized, masked, |m| {
                    rewrite_local_uses(b, specialized, emitted, m)
                });
            }
        }
        Let(decls, body) => {
            let mut binders = Vec::new();
            decl_bound_names(decls, &mut binders);
            with_masked(&binders, specialized, masked, |m| {
                for d in decls.iter_mut() {
                    rewrite_decl_uses_masked(d, specialized, emitted, m);
                }
                rewrite_local_uses(body, specialized, emitted, m);
            });
        }
    }
}

/// Rewrite uses within a decl. The decl's own recursive name is intentionally
/// *not* masked: a self-reference must route to this same specialized copy.
fn rewrite_decl_uses(
    decl: &mut TypedLetDecl,
    specialized: &std::collections::HashSet<Name>,
    emitted: &std::collections::HashSet<String>,
) {
    let mut masked = std::collections::HashSet::new();
    rewrite_decl_uses_masked(decl, specialized, emitted, &mut masked);
}

fn rewrite_decl_uses_masked(
    decl: &mut TypedLetDecl,
    specialized: &std::collections::HashSet<Name>,
    emitted: &std::collections::HashSet<String>,
    masked: &mut std::collections::HashSet<Name>,
) {
    match decl {
        TypedLetDecl::Def { params, body, .. } => {
            let mut binders = Vec::new();
            for (p, _) in params.iter() {
                pattern_bound_names(p, &mut binders);
            }
            with_masked(&binders, specialized, masked, |m| {
                rewrite_local_uses(body, specialized, emitted, m)
            });
        }
        TypedLetDecl::Recursive(defs) => {
            for d in defs {
                rewrite_decl_uses_masked(d, specialized, emitted, masked);
            }
        }
        TypedLetDecl::Destruct(_, value) => {
            rewrite_local_uses(value, specialized, emitted, masked)
        }
    }
}

/// Mangle a `(module, name, concrete type)` triple into a unique symbol.
/// Provisional scheme — readable and injective enough for the types we see;
/// codegen only needs distinct names per instance. The module qualifier keeps
/// specializations from different modules distinct.
pub fn mangle(module: &Name, name: &Name, tipe: &can::Type) -> Name {
    Name::from(format!("{}${}${}", module, name, mangle_type(tipe)))
}

fn mangle_type(tipe: &can::Type) -> String {
    use can::Type::*;
    match tipe {
        // An unconstrained `number` variable defaults to Int — exactly as elm
        // does at the end of inference and as `layout_of` treats it. Without
        // this, a concrete call site (`Int`) and a definition discovered with
        // the variable still unresolved (`number`) mangle to different names
        // for identical code, so the call finds no target.
        Var(n) if n.as_str().starts_with("number") => "Basics$Int".to_string(),
        Var(n) => format!("v{}", n),
        // Qualify by module: two modules can each declare a `Msg`/`Model` whose
        // layouts differ (an enum `i32` in one, a tagged pointer in another).
        // Keyed by the short name alone, their specializations collided to one
        // symbol — invalid IR when the layouts disagree, a silent miscompile
        // when they happen to share a shape. `home` is unique per module (the
        // per-package resolver renames duplicates), so `home$name` is not.
        Type(home, name, args) if args.is_empty() => format!("{}${}", home, name),
        Type(home, name, args) => format!(
            "{}${}${}",
            home,
            name,
            args.iter().map(mangle_type).collect::<Vec<_>>().join("$")
        ),
        Lambda(a, b) => format!("Fn${}${}", mangle_type(a), mangle_type(b)),
        Tuple(a, b, c) => {
            let mut parts = vec![mangle_type(a), mangle_type(b)];
            if let Some(c) = c {
                parts.push(mangle_type(c));
            }
            format!("Tup${}", parts.join("$"))
        }
        Record(fields, _) => format!(
            "Rec${}",
            fields
                .iter()
                .map(|(n, t)| format!("{}_{}", n, mangle_type(t)))
                .collect::<Vec<_>>()
                .join("$")
        ),
        Unit => "Unit".to_string(),
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
        Record(fields, ext) => {
            let mut new_fields: Vec<(Name, can::Type)> = fields
                .iter()
                .map(|(n, t)| (n.clone(), apply_subst(subst, t)))
                .collect();
            // Expand a bound row variable into the record's remaining fields, so
            // a specialized open record becomes the full closed record its
            // concrete layout requires.
            let mut new_ext = ext.clone();
            if let Some(x) = ext {
                if let Some(Record(extra, extra_ext)) = subst.get(x) {
                    let present: std::collections::HashSet<&Name> =
                        new_fields.iter().map(|(n, _)| n).collect();
                    let extra: Vec<(Name, can::Type)> = extra
                        .iter()
                        .filter(|(n, _)| !present.contains(n))
                        .map(|(n, t)| (n.clone(), apply_subst(subst, t)))
                        .collect();
                    new_fields.extend(extra);
                    new_ext = extra_ext.clone();
                }
            }
            Record(new_fields, new_ext)
        }
        Tuple(a, b, c) => Tuple(
            Box::new(apply_subst(subst, a)),
            Box::new(apply_subst(subst, b)),
            c.as_ref().map(|c| Box::new(apply_subst(subst, c))),
        ),
        Unit => Unit,
    }
}

#[cfg(test)]
mod tests {
    use super::mangle;
    use crate::ast::canonical as can;
    use crate::data::Name;

    fn user_type(module: &str, name: &str) -> can::Type {
        can::Type::Type(Name::from(module), Name::from(name), Vec::new())
    }

    #[test]
    fn mangle_qualifies_type_by_module() {
        // Two modules can each declare a same-named type (`Msg`) with different
        // layouts. Keyed by the short name alone their specializations collided
        // to one symbol — invalid IR or a silent miscompile. The mangled type
        // must include the module so they stay distinct.
        let a = mangle(
            &Name::from("Test.Html.Event"),
            &Name::from("expect"),
            &user_type("Bootstrap.ButtonTest", "Msg"),
        );
        let b = mangle(
            &Name::from("Test.Html.Event"),
            &Name::from("expect"),
            &user_type("Bootstrap.AlertTest", "Msg"),
        );
        assert_ne!(a, b);
        assert!(a.as_str().contains("Bootstrap.ButtonTest$Msg"));
    }

    #[test]
    fn mangle_number_default_matches_concrete_int() {
        // A call site with a concrete `Int` and a definition still carrying an
        // unresolved `number` must mangle identically, or the call finds no
        // target.
        let concrete = mangle(
            &Name::from("M"),
            &Name::from("f"),
            &can::Type::Type(Name::from("Basics"), Name::from("Int"), Vec::new()),
        );
        let defaulted = mangle(
            &Name::from("M"),
            &Name::from("f"),
            &can::Type::Var(Name::from("number")),
        );
        assert_eq!(concrete, defaulted);
    }
}
