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
//! This pass does both analysis and specialization across the whole project:
//! [`analyze_project`] discovers the instance set (crossing module boundaries
//! at `VarForeign` references into known modules), and [`specialize_project`]
//! builds a fully typed body for each. Only physical representation is deferred
//! — that is [`crate::ir::layout`]'s job, consulted later by the backend.

use std::collections::HashMap;
use std::rc::Rc;


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
    pub error: Option<String>,
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
    // Seen instances keyed by (module, name, type).
    let mut seen: HashMap<(Name, Name, can::Type), ()> = HashMap::new();
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

/// Default every unresolved `number` variable to Int, as elm does at the end
/// of inference. Instances discovered with `number`s still free (a use whose
/// context never pins them, or a node zonked in a different naming state than
/// its callee's scheme) must specialize CONCRETELY: matching an open scheme
/// against an open instance binds one variable to another under mismatched
/// numbering (`number4 -> Var(number5)`), driving a specialization whose layout
/// disagrees with its callers' (an Int-flavored record where a Float one is
/// expected — invalid IR). `mangle` and `layout_of` already Int-default these
/// names, so defaulting the instance keeps symbol, layout, and body consistent.
pub fn default_numbers(tipe: &can::Type) -> can::Type {
    use can::Type::*;
    match tipe {
        Var(n) if n.as_str().starts_with("number") => {
            Type(Name::from("Basics"), Name::from("Int"), Rc::new(Vec::new()))
        }
        Var(_) | Unit => tipe.clone(),
        Lambda(a, b) => Lambda(
            Rc::new(default_numbers(a)),
            Rc::new(default_numbers(b)),
        ),
        Type(home, name, args) => Type(
            home.clone(),
            name.clone(),
            Rc::new(args.iter().map(default_numbers).collect()),
        ),
        Record(fields, ext) => Record(
            Rc::new(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), default_numbers(t)))
                    .collect(),
            ),
            ext.clone(),
        ),
        Tuple(a, b, c) => Tuple(
            Rc::new(default_numbers(a)),
            Rc::new(default_numbers(b)),
            c.as_ref().map(|c| Rc::new(default_numbers(c))),
        ),
    }
}

/// Type-nesting depth beyond which the monomorphizer gives up — a runaway
/// heuristic, NOT a hard "can't compile deep types" wall. Genuine polymorphic
/// recursion (a recursive call at a strictly deeper type, legal in Elm via an
/// annotation) instantiates unboundedly many types and truly cannot be
/// monomorphized; past this depth we report a clean error instead of hanging.
/// But merely DEEP-yet-FINITE types are fine and must compile: a big record
/// decoded with a `succeed Ctor |> required f1 |> … |> required fN` pipeline
/// builds a `Decoder (a1 -> … -> aN -> Record)` whose nesting depth ≈ N, so the
/// old limit of 20 spuriously rejected records with ~20+ fields (e.g. json-schema
/// via Json.Decode.Pipeline). This bound is generous enough for realistic finite
/// types while still catching true runaway (which generates one instance per
/// depth level, so it errors after ~LIMIT instances — bounded, no hang).
/// (Layout-stationary cases like robinheghan/elm-deque never reach here: they
/// are implemented natively — see `is_native_shunted_module` in `project.rs`.)
const POLY_REC_DEPTH_LIMIT: usize = 200;

/// Node-count beyond which an instance type is treated as un-monomorphizable.
/// Distinct from the DEPTH watchdog: a type can be shallow yet enormous when
/// deeply-nested record/style aliases expand and a type variable bound to one
/// such expansion is substituted into a scheme that mentions it many times
/// (elm-athlete/athlete's `BodyBuilder.computeBlock`: a 3.8k-node scheme whose
/// `a` binds a 97k-node expanded style type, referenced ~9× → an 879k-node
/// instance that OOMs the specializer while it clones the type per body node).
/// Report a clean error instead of exhausting memory (a real risk to the host).
/// The true fix is structural type sharing (interning) so duplicated subtypes
/// don't clone; until then this bounds the damage. Tunable for investigation.
fn spec_type_node_limit() -> usize {
    std::env::var("ALM_SPEC_TYPE_NODE_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400_000)
}

fn poly_rec_error(module: &Name, name: &Name) -> String {
    format!(
        "`{}.{}` uses polymorphic recursion (each recursive call at a more deeply nested type), which the native backend's monomorphizer cannot yet compile.",
        module, name
    )
}

fn type_too_large_error(module: &Name, name: &Name, limit: usize) -> String {
    format!(
        "`{module}.{name}` expands to a type with over {limit} nodes, which the \
         monomorphizer cannot compile without exhausting memory.\n\n\
         This is triggered by using a function whose type involves deeply-nested \
         type aliases (records inside records — e.g. style or attribute types) at \
         a FULLY GENERIC type. The aliases are expanded at every occurrence, so a \
         type that is small when shared balloons into millions of duplicated \
         nodes. The very same function compiles fine when it is used at a \
         CONCRETE type.\n\n\
         To fix: constrain the type at the use site with a type annotation so \
         `{name}`'s type variables become concrete (e.g. annotate the value or \
         the surrounding function) instead of leaving it fully polymorphic. \
         Referencing `{module}.{name}` only as a generic value (never applied to \
         concrete arguments) is what forces the blow-up."
    )
}

/// Whether `tipe` has more than `limit` nodes counted LOGICALLY (each occurrence
/// of a shared subtree counts, no dedup). Interning shares storage, but many
/// operations — above all `mangle`, which spells the type out in full — still
/// cost O(logical size), so the logical count is the metric that actually bounds
/// compile cost. A type that is structurally small yet logically enormous
/// (deeply-nested aliases duplicated at every occurrence) still trips this and
/// is reported as an error rather than exhausting time/memory. Stops at the
/// limit, so it is O(min(logical, limit)).
fn type_exceeds_nodes(tipe: &can::Type, limit: usize) -> bool {
    fn go(t: &can::Type, n: &mut usize, limit: usize) -> bool {
        *n += 1;
        if *n > limit {
            return true;
        }
        use can::Type::*;
        match t {
            Var(_) | Unit => false,
            Lambda(a, b) => go(a, n, limit) || go(b, n, limit),
            Type(_, _, args) => args.iter().any(|a| go(a, n, limit)),
            Tuple(a, b, c) => {
                go(a, n, limit) || go(b, n, limit) || c.as_ref().map_or(false, |c| go(c, n, limit))
            }
            Record(fields, _) => fields.iter().any(|(_, t)| go(t, n, limit)),
        }
    }
    let mut n = 0;
    go(tipe, &mut n, limit)
}

fn enqueue(
    queue: &mut Vec<Instance>,
    seen: &mut HashMap<(Name, Name, can::Type), ()>,
    set: &mut MonoSet,
    instance: Instance,
) {
    if type_depth(&instance.tipe) > POLY_REC_DEPTH_LIMIT {
        if set.error.is_none() {
            set.error = Some(poly_rec_error(&instance.module, &instance.name));
        }
        return;
    }
    let node_limit = spec_type_node_limit();
    if type_exceeds_nodes(&instance.tipe, node_limit) {
        if set.error.is_none() {
            set.error = Some(type_too_large_error(&instance.module, &instance.name, node_limit));
        }
        return;
    }
    // Key by the type VALUE, not its Debug rendering: formatting every instance
    // type is O(size) and dominated compile time on instantiation-heavy
    // packages (elm-geometry burned minutes here).
    let key = (
        instance.module.clone(),
        instance.name.clone(),
        instance.tipe.clone(),
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
            for (a, b) in args1.iter().zip(args2.iter()) {
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
            for (name, t1) in f1.iter() {
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
                    .or_insert_with(|| Record(Rc::new(extra), None));
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
    /// A fatal specialization diagnosis (e.g. polymorphic recursion driving
    /// unbounded instantiation) — reported instead of hanging or blowing the
    /// compiler's stack.
    pub error: Option<String>,
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
    /// A built-in/kernel value. The typed backend compiles known ones to
    /// specialized inline code (`gen_kernel`) and falls back to the uniform
    /// runtime closure for the rest.
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
    let analyzed = analyze_project(modules, entry);
    let mut error = analyzed.error.clone();
    for inst in analyzed.instances {
        if seen.insert(mangle(&inst.module, &inst.name, &inst.tipe).to_string()) {
            worklist.push(inst);
        }
    }
    let mut wi = 0;
    while wi < worklist.len() && error.is_none() {
        let mut instance = worklist[wi].clone();
        // Watchdog: specialization discovers further instances (via `sink`), so
        // a polymorphic recursion can still surface here even when the analysis
        // seed was shallow. See `POLY_REC_DEPTH_LIMIT`.
        if type_depth(&instance.tipe) > POLY_REC_DEPTH_LIMIT {
            error = Some(poly_rec_error(&instance.module, &instance.name));
            break;
        }
        // Node-count watchdog (bloated-alias types, see `spec_type_node_limit`):
        // caught here before `spec.expr` clones the type across every body node.
        let node_limit = spec_type_node_limit();
        if type_exceeds_nodes(&instance.tipe, node_limit) {
            error = Some(type_too_large_error(&instance.module, &instance.name, node_limit));
            break;
        }
        // Specialize at the number-defaulted type (see `default_numbers`); the
        // reference sites' mangled names already Int-default, so the symbol,
        // the layout, and the copy compiled here all agree.
        instance.tipe = default_numbers(&instance.tipe);
        wi += 1;
        let Some(mctx) = ctxs.get(&instance.module) else {
            continue;
        };
        let Some(def) = mctx.defs.get(&instance.name) else {
            continue;
        };
        let mangled = mangle(&instance.module, &instance.name, &instance.tipe);
        if std::env::var("ALM_MONO_TRACE").map_or(false, |t| t == instance.name.as_str()) {
            eprintln!(
                "[mono spec {}] inst={:?}\n   scheme={:?}",
                instance.name.as_str(),
                instance.tipe,
                mctx.types.get(&instance.name)
            );
        }
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
                    params.push((arg.clone(), (*a).clone()));
                    remaining = (*b).clone();
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
            remaining = (*b).clone();
            idx += 1;
        }
        functions.push(TypedFn {
            mangled,
            original: instance.name.clone(),
            module: instance.module.clone(),
            tipe: instance.tipe.clone(),
            params,
            body: Reducer::default().opt(body),
            region: def.name.region,
        });
        // Enqueue every instance this body's references demanded.
        for inst in sink.borrow_mut().drain(..) {
            if seen.insert(mangle(&inst.module, &inst.name, &inst.tipe).to_string()) {
                worklist.push(inst);
            }
        }
    }

    MonoProgram { functions, error }
}

/// Beta-reduces statically-known applications so higher-order glue over
/// non-word values (records/tuples) compiles to direct calls instead of
/// building and applying a boxed closure. A closure's uniform calling
/// convention passes every argument as one machine word, so a multi-word value
/// (a record like elm-flate's `BitWriter`, a tuple) is heap-boxed on the way in
/// and unboxed on the way out — per application, which is catastrophic in a hot
/// loop (`Symbol.encode` threads a `BitWriter` through `maybeExtra`/
/// `maybeDistance` per compressed symbol).
///
/// The reductions, all effect-preserving (Elm is pure):
///   - `x |> f` / `f <| x`      -> apply `f` to `x`
///   - `identity x`             -> `x`
///   - `always x _`             -> `x`
///   - `(f >> g) x`             -> `g (f x)`;  `(f << g) x` -> `f (g x)`
///   - `(f a…) b…`              -> `f a… b…`   (currying is associative)
///   - `(case s of … -> fᵢ) x` -> `case s of … -> fᵢ x` (args hoisted so the
///                                 scrutinee and each argument evaluate once)
///   - a zero-parameter function-typed `let` binding is inlined at application
///     sites, so the cases above fire on it (its now-dead binding is DCE'd).
/// Anything not matched stays an ordinary `Call`, which the backend compiles as
/// before. Lambdas are left as calls — the backend already inlines a saturated
/// lambda application without boxing.
#[derive(Default)]
struct Reducer {
    /// Zero-parameter, function-typed local bindings, inlined where applied.
    inlinable: HashMap<Name, TypedExpr>,
    fresh: usize,
}

impl Reducer {
    fn opt(&mut self, e: TypedExpr) -> TypedExpr {
        let kind = match e.kind {
            // `l |> r` is `r l`; `l <| r` is `l r`.
            TypedKind::Binop(ref op, ..) if op.as_str() == "|>" || op.as_str() == "<|" => {
                let (op, _h, _f, l, r) = match e.kind {
                    TypedKind::Binop(op, h, f, l, r) => (op, h, f, l, r),
                    _ => unreachable!(),
                };
                let (l, r) = (self.opt(*l), self.opt(*r));
                let (func, arg) = if op.as_str() == "|>" { (r, l) } else { (l, r) };
                return self.apply(func, vec![arg], e.tipe, e.region);
            }
            TypedKind::Call(f, args) => {
                let f = self.opt(*f);
                let args = args.into_iter().map(|a| self.opt(a)).collect();
                return self.apply(f, args, e.tipe, e.region);
            }
            TypedKind::Let(decls, body) => {
                // Optimize each declaration; record zero-param function-typed
                // Defs as inlinable for this let's scope, then restore.
                let mut added = Vec::new();
                let decls: Vec<TypedLetDecl> = decls
                    .into_iter()
                    .map(|d| self.opt_decl(d, &mut added))
                    .collect();
                let body = self.opt(*body);
                for n in added {
                    self.inlinable.remove(&n);
                }
                TypedKind::Let(decls, Box::new(body))
            }
            TypedKind::Case(scrut, branches) => TypedKind::Case(
                Box::new(self.opt(*scrut)),
                branches.into_iter().map(|(p, b)| (p, self.opt(b))).collect(),
            ),
            TypedKind::If(branches, otherwise) => TypedKind::If(
                branches
                    .into_iter()
                    .map(|(c, b)| (self.opt(c), self.opt(b)))
                    .collect(),
                Box::new(self.opt(*otherwise)),
            ),
            TypedKind::Lambda(ps, b) => TypedKind::Lambda(ps, Box::new(self.opt(*b))),
            TypedKind::List(xs) => TypedKind::List(xs.into_iter().map(|x| self.opt(x)).collect()),
            TypedKind::Negate(x) => TypedKind::Negate(Box::new(self.opt(*x))),
            TypedKind::Binop(op, h, f, l, r) => {
                TypedKind::Binop(op, h, f, Box::new(self.opt(*l)), Box::new(self.opt(*r)))
            }
            TypedKind::Access(r, n) => TypedKind::Access(Box::new(self.opt(*r)), n),
            TypedKind::Update(r, fs) => TypedKind::Update(
                Box::new(self.opt(*r)),
                fs.into_iter().map(|(n, v)| (n, self.opt(v))).collect(),
            ),
            TypedKind::Record(fs) => {
                TypedKind::Record(fs.into_iter().map(|(n, v)| (n, self.opt(v))).collect())
            }
            TypedKind::Tuple(a, b, c) => TypedKind::Tuple(
                Box::new(self.opt(*a)),
                Box::new(self.opt(*b)),
                c.map(|c| Box::new(self.opt(*c))),
            ),
            other => other,
        };
        TypedExpr { tipe: e.tipe, kind, region: e.region }
    }

    fn opt_decl(&mut self, d: TypedLetDecl, added: &mut Vec<Name>) -> TypedLetDecl {
        match d {
            TypedLetDecl::Def { name, params, body } => {
                let body = self.opt(body);
                // A `let f = <function-valued expr>` (no parameters of its own):
                // inline it at its application sites so pipes/compositions/cases
                // in its body reduce there.
                if params.is_empty() && matches!(body.tipe, can::Type::Lambda(..)) {
                    self.inlinable.insert(name.clone(), body.clone());
                    added.push(name.clone());
                }
                TypedLetDecl::Def { name, params, body }
            }
            TypedLetDecl::Recursive(defs) => TypedLetDecl::Recursive(
                defs.into_iter().map(|d| self.opt_decl(d, added)).collect(),
            ),
            TypedLetDecl::Destruct(p, v) => TypedLetDecl::Destruct(p, self.opt(v)),
        }
    }

    /// Apply `func` to `args`, reducing structurally where possible. `tipe` is
    /// the type of the whole application (used only as a fallback).
    fn apply(
        &mut self,
        func: TypedExpr,
        mut args: Vec<TypedExpr>,
        tipe: can::Type,
        region: Region,
    ) -> TypedExpr {
        if args.is_empty() {
            return func;
        }
        let result_ty = peel_arrows_ty(&func.tipe, args.len()).unwrap_or_else(|| tipe.clone());
        match func.kind {
            // Currying is associative: `(f a…) b…` == `f a… b…`.
            TypedKind::Call(f, mut a1) => {
                a1.append(&mut args);
                self.apply(*f, a1, tipe, region)
            }
            TypedKind::Foreign(ref m, ref n)
                if m.as_str() == "Basics" && n.as_str() == "identity" && args.len() == 1 =>
            {
                args.pop().unwrap()
            }
            TypedKind::Foreign(ref m, ref n)
                if m.as_str() == "Basics" && n.as_str() == "always" && args.len() >= 2 =>
            {
                let x = args.remove(0);
                args.remove(0); // the ignored argument
                self.apply(x, args, tipe, region)
            }
            // `(f >> g) x` = `g (f x)`; `(f << g) x` = `f (g x)`.
            TypedKind::Binop(ref op, .., ref f, ref g)
                if (op.as_str() == ">>" || op.as_str() == "<<") && args.len() == 1 =>
            {
                let compose_r = op.as_str() == ">>";
                let (f, g) = (f.as_ref().clone(), g.as_ref().clone());
                let (first, second) = if compose_r { (f, g) } else { (g, f) };
                let fx = self.apply(first, args, tipe.clone(), region);
                self.apply(second, vec![fx], tipe, region)
            }
            // Nested pipes appearing directly in function position.
            TypedKind::Binop(ref op, .., ref l, ref r)
                if op.as_str() == "|>" || op.as_str() == "<|" =>
            {
                let pipe_r = op.as_str() == "|>";
                let (l, r) = (l.as_ref().clone(), r.as_ref().clone());
                let (func2, arg2) = if pipe_r { (r, l) } else { (l, r) };
                let inner = self.apply(func2, vec![arg2], func.tipe.clone(), region);
                self.apply(inner, args, tipe, region)
            }
            TypedKind::Local(ref name) if self.inlinable.contains_key(name) => {
                let body = self.inlinable[name].clone();
                self.apply(body, args, tipe, region)
            }
            // Push the application into every branch, hoisting the arguments (and
            // scrutinee/condition, already evaluated) so they run once.
            TypedKind::Case(scrut, branches) => {
                let (bindings, args) = self.hoist(args);
                let branches = branches
                    .into_iter()
                    .map(|(p, b)| (p, self.apply(b, args.clone(), result_ty.clone(), region)))
                    .collect();
                let case = TypedExpr {
                    tipe: result_ty,
                    kind: TypedKind::Case(scrut, branches),
                    region,
                };
                wrap_let(bindings, case, region)
            }
            TypedKind::If(branches, otherwise) => {
                let (bindings, args) = self.hoist(args);
                let branches = branches
                    .into_iter()
                    .map(|(c, b)| (c, self.apply(b, args.clone(), result_ty.clone(), region)))
                    .collect();
                let otherwise = self.apply(*otherwise, args, result_ty.clone(), region);
                let iff = TypedExpr {
                    tipe: result_ty,
                    kind: TypedKind::If(branches, Box::new(otherwise)),
                    region,
                };
                wrap_let(bindings, iff, region)
            }
            TypedKind::Let(decls, body) => {
                // Hoist args out so a let binding can't shadow their free vars.
                let (bindings, args) = self.hoist(args);
                let body = self.apply(*body, args, result_ty.clone(), region);
                let inner = TypedExpr {
                    tipe: result_ty,
                    kind: TypedKind::Let(decls, Box::new(body)),
                    region,
                };
                wrap_let(bindings, inner, region)
            }
            _ => TypedExpr {
                tipe: result_ty,
                kind: TypedKind::Call(Box::new(func), args),
                region,
            },
        }
    }

    /// Bind each non-trivial argument to a fresh local so it evaluates once when
    /// the application is duplicated across branches; trivial arguments (a
    /// variable, literal, or reference) pass through unbound.
    fn hoist(&mut self, args: Vec<TypedExpr>) -> (Vec<TypedLetDecl>, Vec<TypedExpr>) {
        let mut bindings = Vec::new();
        let mut out = Vec::new();
        for a in args {
            if is_trivial(&a) {
                out.push(a);
            } else {
                let name = Name::from(format!("_hoist{}", self.fresh));
                self.fresh += 1;
                out.push(TypedExpr {
                    tipe: a.tipe.clone(),
                    kind: TypedKind::Local(name.clone()),
                    region: a.region,
                });
                bindings.push(TypedLetDecl::Def { name, params: Vec::new(), body: a });
            }
        }
        (bindings, out)
    }
}

/// Whether re-evaluating `e` is free of cost and effect, so it can be
/// duplicated across branches without hoisting.
fn is_trivial(e: &TypedExpr) -> bool {
    matches!(
        e.kind,
        TypedKind::Local(_)
            | TypedKind::Global(_)
            | TypedKind::Foreign(..)
            | TypedKind::Int(_)
            | TypedKind::Float(_)
            | TypedKind::Str(_)
            | TypedKind::Chr(_)
            | TypedKind::Unit
            | TypedKind::Accessor(_)
    )
}

/// Wrap `body` in a `let` binding `bindings`, or return it unchanged if empty.
fn wrap_let(bindings: Vec<TypedLetDecl>, body: TypedExpr, region: Region) -> TypedExpr {
    if bindings.is_empty() {
        body
    } else {
        TypedExpr {
            tipe: body.tipe.clone(),
            kind: TypedKind::Let(bindings, Box::new(body)),
            region,
        }
    }
}

/// The result type of applying a function of type `tipe` to `n` arguments:
/// peel `n` leading `->` arrows. `None` if it is not a function of that arity.
fn peel_arrows_ty(tipe: &can::Type, n: usize) -> Option<can::Type> {
    let mut t = tipe;
    for _ in 0..n {
        match t {
            can::Type::Lambda(_, b) => t = b,
            _ => return None,
        }
    }
    Some(t.clone())
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
        let captured = match self.ctx.node_types.get(&expr.region) {
            Some(t) => t.clone(),
            None => {
                if std::env::var("ALM_MISSING_NODE").is_ok() {
                    eprintln!(
                        "[missing-node] {}:{}..{}:{} kind={:?}",
                        expr.region.start.row,
                        expr.region.start.col,
                        expr.region.end.row,
                        expr.region.end.col,
                        std::mem::discriminant(&expr.value)
                    );
                }
                can::Type::Unit
            }
        };
        apply_subst(subst, &captured)
    }

    fn expr(&self, expr: &can::Expr, subst: &HashMap<Name, can::Type>) -> TypedExpr {
        use can::Expr_::*;
        let tipe = self.node_ty(expr, subst);
        let kind = match &expr.value {
            // A `number` literal whose concrete type resolves to Float IS a
            // float (elm defaults unresolved `number` to Int, but a literal in
            // a Float position — `duration 150`, `( x, y, 0 )` — must take the
            // resolved type). Without this the caller passes an i64 where the
            // specialized callee expects a double: invalid IR when LLVM checks
            // the call, silently reinterpreted bits when it does not.
            Int(n) => match &tipe {
                can::Type::Type(home, name, _)
                    if home.as_str() == "Basics" && name.as_str() == "Float" =>
                {
                    TypedKind::Float(*n as f64)
                }
                _ => TypedKind::Int(*n),
            },
            Float(f) => TypedKind::Float(*f),
            Str(s) => TypedKind::Str(s.clone()),
            Chr(c) => TypedKind::Chr(*c),
            Unit => TypedKind::Unit,
            // WebGL shaders are JS-backend only; the native/typed path never
            // renders them. Carry the source so the node is well-formed.
            Shader(shader) => TypedKind::Str(shader.src.clone()),
            VarLocal(name) => TypedKind::Local(name.clone()),
            VarTopLevel(name) => {
                if std::env::var("ALM_MONO_TRACE").map_or(false, |t| t == name.as_str()) {
                    eprintln!(
                        "[mono {} @{}:{}] {:?}",
                        name.as_str(),
                        expr.region.start.row,
                        expr.region.start.col,
                        tipe
                    );
                }
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
                // ALM_MONO_TRACE=<name> prints every instantiation of a
                // definition during specialization (debugging aid).
                if std::env::var("ALM_MONO_TRACE").map_or(false, |t| t == name.as_str()) {
                    eprintln!(
                        "[mono {} @{}:{}] {:?}",
                        name.as_str(),
                        expr.region.start.row,
                        expr.region.start.col,
                        tipe
                    );
                }
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
                        Rc::new(lt.tipe.clone()),
                        Rc::new(can::Type::Lambda(
                            Rc::new(rt.tipe.clone()),
                            Rc::new(tipe.clone()),
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
                            params.push((arg.clone(), (*a).clone()));
                            remaining = (*b).clone();
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
                        can::Type::Lambda(_, b) => covered = (*b).clone(),
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
                    covered = (*b).clone();
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
        let mut poly_index: HashMap<usize, Vec<Name>> = HashMap::new();
        for (i, decl) in decls.iter().enumerate() {
            // Any local binding whose type is polymorphic in context needs
            // per-use-site specialization -- including a *point-free* one
            // (zero args), e.g. `f = List.sort << g`. Compiling such a
            // binding once under the enclosing substitution leaves its type
            // variable free, producing a boxed (`Ref`) specialization of the
            // functions it composes; a concrete unboxed call site then reads
            // those values at the wrong layout. The `type_has_free_tyvar`
            // check below filters out monomorphic values (`x = 5`).
            //
            // A `Recursive` group registers EVERY member — including mutual
            // recursion (Form.Tree's `walkTree`/`mapGroupItem`): compiling a
            // mutual group once under the enclosing substitution has the same
            // wrong-layout consequence, and the members must specialize
            // together since they call each other.
            let group: Vec<(&can::Def, bool)> = match decl {
                can::LetDecl::Def(def) => vec![(def, false)],
                // KNOWN GAP: a lambda-style member (`f = \x -> ...`,
                // empty `def.args`) skips the whole group. Admitting such
                // members by TYPE was tried TWICE (incl. after the
                // runtime-merge GC fix) and reliably breaks
                // elm-monocle/elm-statecharts/intervals — zero-arg member
                // specialization (local_def_params_body eta-normalization
                // or downstream assembly) needs real work first.
                can::LetDecl::Recursive(defs)
                    if !defs.is_empty() && defs.iter().all(|d| !d.args.is_empty()) =>
                {
                    defs.iter().map(|d| (d, true)).collect()
                }
                _ => continue,
            };
            let mut members: Vec<(&can::Def, bool, can::Type)> = Vec::new();
            let mut any_poly = false;
            let mut complete = true;
            for (def, recursive) in &group {
                let Some(generic) = self.ctx.node_types.get(&def.name.region) else {
                    complete = false;
                    break;
                };
                let subbed = apply_subst(subst, generic);
                // A single definition keeps the wider rule (number vars count —
                // see type_has_free_tyvar's comment); a MUTUAL group only
                // specializes for genuine type variables (see
                // type_has_free_named_tyvar).
                let is_poly = if group.len() == 1 {
                    type_has_free_tyvar(&subbed)
                } else {
                    type_has_free_named_tyvar(&subbed)
                };
                if is_poly {
                    any_poly = true;
                }
                members.push((def, *recursive, generic.clone()));
            }
            if !complete || !any_poly {
                continue;
            }
            let mut names = Vec::new();
            for (def, recursive, generic) in members {
                names.push(def.name.value.clone());
                polys.insert(
                    def.name.value.clone(),
                    PolyLocal {
                        def,
                        generic,
                        recursive,
                    },
                );
            }
            poly_index.insert(i, names);
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
            if std::env::var("ALM_SPECLET_TRACE").is_ok() {
                eprintln!(
                    "[speclet {}] use-ty={:?}\n   g_sub={:?}\n   mangled={}",
                    name.as_str(), ty, g_sub, mangled.as_str()
                );
            }
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
            spec_decls.insert(mangled.to_string(), inner);
            per_name.entry(name).or_default().push(mangled);
        }

        // Assemble the output decls, preserving the original order: each poly
        // local expands in place into its specialized copies; a poly local with
        // no discovered use falls back to the ordinary single copy.
        let mut out_decls: Vec<TypedLetDecl> = Vec::new();
        let mut specialized: std::collections::HashSet<Name> = std::collections::HashSet::new();
        for (i, decl) in decls.iter().enumerate() {
            match poly_index.get(&i) {
                Some(names) => {
                    let mut copies: Vec<TypedLetDecl> = Vec::new();
                    for name in names {
                        if let Some(mangleds) = per_name.get(name) {
                            for m in mangleds {
                                copies.push(spec_decls.remove(m.as_str()).unwrap());
                            }
                        }
                    }
                    if copies.is_empty() {
                        // No discovered use of any member: ordinary single copy.
                        out_decls.push(self.let_decl(decl, subst));
                        continue;
                    }
                    for name in names {
                        if per_name.get(name).map_or(false, |ms| !ms.is_empty()) {
                            specialized.insert(name.clone());
                        }
                    }
                    let recursive = names
                        .first()
                        .map_or(false, |n| polys[n].recursive);
                    if !recursive {
                        out_decls.extend(copies);
                    } else if names.len() > 1 {
                        // A MUTUAL group: all specialized copies go into ONE
                        // Recursive decl so every copy sees every other (they
                        // reference each other's mangled names, and
                        // let-bindings otherwise scope sequentially).
                        out_decls.push(TypedLetDecl::Recursive(copies));
                    } else {
                        // A single self-recursive local: its copies never
                        // reference each other, so each stays its own
                        // singleton group (the mutual-group machinery's
                        // heavier env layout is unnecessary and was observed
                        // to miscompile independent copies bundled together).
                        for c in copies {
                            out_decls.push(TypedLetDecl::Recursive(vec![c]));
                        }
                    }
                }
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
                    remaining = Some((*b).clone());
                    (*a).clone()
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
            remaining = Some((*b).clone());
            idx += 1;
        }
        (params, body)
    }
}

/// The constraint-class prefixes of Elm's super-type variables. Treated by
/// class rather than spelling where it matters: `alpha_normalize` mangles them
/// by class, and `type_has_free_named_tyvar` ignores them.
const CONSTRAINT_PREFIXES: [&str; 4] = ["number", "comparable", "appendable", "compappend"];

/// Whether `tipe` contains a type variable that `var_is_free` accepts, or an
/// open record (a free row variable, whose concrete field set — and therefore
/// the sorted offsets a record access compiles to — is not yet known).
fn type_has_free_var(tipe: &can::Type, var_is_free: &impl Fn(&Name) -> bool) -> bool {
    use can::Type::*;
    match tipe {
        Var(name) => var_is_free(name),
        Lambda(a, b) => type_has_free_var(a, var_is_free) || type_has_free_var(b, var_is_free),
        Type(_, _, args) => args.iter().any(|t| type_has_free_var(t, var_is_free)),
        Tuple(a, b, c) => {
            type_has_free_var(a, var_is_free)
                || type_has_free_var(b, var_is_free)
                || c.as_ref().map_or(false, |c| type_has_free_var(c, var_is_free))
        }
        Record(fields, ext) => {
            ext.is_some() || fields.iter().any(|(_, t)| type_has_free_var(t, var_is_free))
        }
        Unit => false,
    }
}

/// Like [`type_has_free_tyvar`], but `number`/`comparable`-style super
/// variables do not count. Used to gate MUTUAL-group specialization: a group
/// that is polymorphic only in `number` compiled correctly before group
/// specialization existed (every use defaults to Int uniformly), and
/// specializing such groups interacts badly with the top-level number-default
/// fixpoint (Int/Float instantiations of the group members enqueue each other
/// unboundedly). A genuinely-polymorphic group (a real type variable) is the
/// case group specialization exists to fix.
fn type_has_free_named_tyvar(tipe: &can::Type) -> bool {
    type_has_free_var(tipe, &|n| {
        !CONSTRAINT_PREFIXES.iter().any(|p| n.as_str().starts_with(p))
    })
}

/// Whether `tipe` has any free variable at all. `number` variables count: a
/// generalized local used at both Int and Float (or pinned to Float only by a
/// LATER consumer) needs one copy per numeric instantiation, because natively
/// Int and Float have different layouts (on JS they share one representation).
fn type_has_free_tyvar(tipe: &can::Type) -> bool {
    type_has_free_var(tipe, &|_| true)
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
    Name::from(format!("{}${}", name, mangle_type(&alpha_normalize(tipe))))
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
                } else {
                    // The use is at a type carrying a free variable the fixpoint
                    // never instantiated concretely (e.g. a phantom `state` param:
                    // `done` returns `P.Done x : Step state a` for ANY state). Such
                    // a variable can't affect the value's layout — a layout-relevant
                    // one would have been constrained and seen — so any existing
                    // specialization of `n` is runtime-compatible (all boxed eqref
                    // in wasm-gc). Route to one so the reference resolves instead of
                    // dangling. (A package whose exact mangle IS emitted never
                    // reaches this branch, so it can't regress a working build.)
                    let prefix = format!("{}$", n.as_str());
                    if let Some(any) = emitted.iter().find(|e| e.starts_with(&prefix)) {
                        *n = Name::from(any.as_str());
                    }
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

/// Structural nesting depth of a type — the watchdog metric for runaway
/// specialization (polymorphic recursion instantiates `Deque (Buffer^n a)`
/// for unbounded n; genuine programs stay shallow).
fn type_depth(tipe: &can::Type) -> usize {
    use can::Type::*;
    match tipe {
        Var(_) | Unit => 1,
        Lambda(a, b) => 1 + type_depth(a).max(type_depth(b)),
        Type(_, _, args) => 1 + args.iter().map(type_depth).max().unwrap_or(0),
        Tuple(a, b, c) => {
            1 + type_depth(a)
                .max(type_depth(b))
                .max(c.as_deref().map(type_depth).unwrap_or(0))
        }
        Record(fields, _) => 1 + fields.iter().map(|(_, t)| type_depth(t)).max().unwrap_or(0),
    }
}

/// Mangle a `(module, name, concrete type)` triple into a unique symbol.
/// The encoding must be INJECTIVE — a collision silently merges two
/// specializations of different layouts (a miscompile). See `mangle_type`
/// for the prefix-unambiguous scheme. The module qualifier keeps
/// specializations from different modules distinct.
pub fn mangle(module: &Name, name: &Name, tipe: &can::Type) -> Name {
    Name::from(format!("{}${}${}", module, name, mangle_type(&alpha_normalize(tipe))))
}

/// Rename type variables to `p0, p1, …` in first-occurrence order. Two
/// instantiations that differ only in tyvar NAMES (`Deque (Buffer a17)` vs
/// `Deque (Buffer a18)` — inference mints fresh names per use) are the same
/// specialization; without this the fixpoint enqueues unboundedly many
/// name-distinct copies (elm-deque's polymorphic recursion compiled forever
/// at shallow types), and layout-identical `Ref` copies duplicated code.
fn alpha_normalize(tipe: &can::Type) -> can::Type {
    fn go(t: &can::Type, map: &mut Vec<(Name, Name)>) -> can::Type {
        use can::Type::*;
        match t {
            Var(n) => {
                // Keep constraint prefixes meaningful: `number*`/`comparable*`
                // mangle by their constraint class, not their spelling.
                let class = CONSTRAINT_PREFIXES
                    .iter()
                    .find(|c| n.as_str().starts_with(*c));
                if let Some(c) = class {
                    return Var(Name::from(*c));
                }
                if let Some((_, to)) = map.iter().find(|(from, _)| from == n) {
                    return Var(to.clone());
                }
                let to = Name::from(format!("p{}", map.len()));
                map.push((n.clone(), to.clone()));
                Var(to)
            }
            Lambda(a, b) => Lambda(Rc::new(go(a, map)), Rc::new(go(b, map))),
            Type(h, n, args) => Type(h.clone(), n.clone(), Rc::new(args.iter().map(|a| go(a, map)).collect())),
            Tuple(a, b, c) => Tuple(
                Rc::new(go(a, map)),
                Rc::new(go(b, map)),
                c.as_ref().map(|x| Rc::new(go(x, map))),
            ),
            Record(fields, ext) => {
                // Number type variables in the SAME field order `mangle_type`
                // renders (sorted by name). Node types and row-variable
                // expansion can present a record's fields in different orders;
                // numbering vars by the raw traversal order then makes two
                // alpha-EQUAL record types number their vars differently, so
                // they mangle to distinct strings — and a use resolves to a
                // specialization created under the other name (the elm-charts
                // `stack$…vp0…vp1…` vs `…vp1…vp0…` unbound-local bug). Sorting
                // here keeps the var numbering canonical with the render order.
                let mut sorted: Vec<&(Name, can::Type)> = fields.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                Record(
                    Rc::new(sorted.iter().map(|(n, t)| (n.clone(), go(t, map))).collect()),
                    ext.clone(),
                )
            }
            Unit => Unit,
        }
    }
    let mut map = Vec::new();
    go(tipe, &mut map)
}

fn mangle_type(tipe: &can::Type) -> String {
    use can::Type::*;
    match tipe {
        // An unconstrained `number` variable defaults to Int — exactly as elm
        // does at the end of inference and as `layout_of` treats it. Without
        // this, a concrete call site (`Int`) and a definition discovered with
        // the variable still unresolved (`number`) mangle to different names
        // for identical code, so the call finds no target.
        Var(n) if n.as_str().starts_with("number") => "Basics$Int$0".to_string(),
        Var(n) => format!("v{}", n),
        // Qualify by module: two modules can each declare a `Msg`/`Model` whose
        // layouts differ (an enum `i32` in one, a tagged pointer in another).
        // Keyed by the short name alone, their specializations collided to one
        // symbol — invalid IR when the layouts disagree, a silent miscompile
        // when they happen to share a shape. `home` is unique per module (the
        // per-package resolver renames duplicates), so `home$name` is not.
        // Every VARIABLE-ARITY form carries its child COUNT (`Fn` needs none
        // — Lambda is fixed arity 2), making the encoding a
        // prefix-unambiguous Polish notation. Without counts the flat `$`
        // joining cannot tell nesting from siblings: `Rec$a_T$b_U` followed by
        // a sibling `$c_V` is byte-identical to a Rec whose LAST FIELD contains
        // `c_V` nested one level deeper — two different types (html-table's
        // `{colA,id,items:List {colB,id,things:…}}` vs the same fields with
        // `things` hoisted to the outer record) collided to one specialization,
        // and callers handed it rows of the other layout.
        Type(home, name, args) if args.is_empty() => format!("{}${}$0", home, name),
        Type(home, name, args) => format!(
            "{}${}${}${}",
            home,
            name,
            args.len(),
            args.iter().map(mangle_type).collect::<Vec<_>>().join("$")
        ),
        Lambda(a, b) => format!("Fn${}${}", mangle_type(a), mangle_type(b)),
        Tuple(a, b, c) => {
            let mut parts = vec![mangle_type(a), mangle_type(b)];
            if let Some(c) = c {
                parts.push(mangle_type(c));
            }
            format!("Tup{}${}", parts.len(), parts.join("$"))
        }
        Record(fields, _) => {
            // Field order is not canonical in node types (row-variable
            // expansion appends extension fields after the named ones, while
            // fully-inferred nodes carry them sorted), but the layout sorts by
            // name — so mangle sorted too, or one record type mangles to two
            // names and a reference resolves to a missing sibling copy.
            // Field name and type as separate `$` tokens: `_` is legal in
            // field and tyvar names, so the old `name_type` form collided
            // (`{f : oo_vx}` vs `{f_voo : x}`). Sorted by field name (unique
            // per record) to stay canonical with the layout's order.
            let mut sorted: Vec<&(Name, can::Type)> = fields.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let parts: Vec<String> = sorted
                .iter()
                .map(|(n, t)| format!("{}${}", n, mangle_type(t)))
                .collect();
            format!("Rec{}${}", parts.len(), parts.join("$"))
        }
        Unit => "Unit".to_string(),
    }
}

/// Apply a substitution to a type, replacing bound variables.
///
/// Sharing-preserving: a subtree that no substitution touches is returned
/// UNCHANGED (an O(1) `Rc` clone), not rebuilt. This is essential after
/// interning — `spec.expr` calls this per body node, and a naive rebuild would
/// re-materialize a large shared type (elm-athlete's style records) on every
/// call, re-inflating the very duplication interning removes. `subst_shared`
/// returns `None` for "unchanged" so callers can reuse the original `Rc`.
fn apply_subst(subst: &HashMap<Name, can::Type>, tipe: &can::Type) -> can::Type {
    subst_shared(subst, tipe).unwrap_or_else(|| tipe.clone())
}

/// The substitution workhorse. Returns `None` when `tipe` is unchanged (nothing
/// in it was substituted), letting the caller keep the shared original.
fn subst_shared(subst: &HashMap<Name, can::Type>, tipe: &can::Type) -> Option<can::Type> {
    use can::Type::*;
    // An unchanged child: reuse the shared `Rc` (O(1)); a changed one: box the
    // new type. `changed` tracks whether any child actually differed.
    fn child(subst: &HashMap<Name, can::Type>, rc: &Rc<can::Type>, changed: &mut bool) -> Rc<can::Type> {
        match subst_shared(subst, rc) {
            Some(t) => {
                *changed = true;
                Rc::new(t)
            }
            None => rc.clone(),
        }
    }
    match tipe {
        Var(name) => subst.get(name).cloned(),
        Unit => None,
        Lambda(a, b) => {
            let mut changed = false;
            let na = child(subst, a, &mut changed);
            let nb = child(subst, b, &mut changed);
            changed.then(|| Lambda(na, nb))
        }
        Tuple(a, b, c) => {
            let mut changed = false;
            let na = child(subst, a, &mut changed);
            let nb = child(subst, b, &mut changed);
            let nc = c.as_ref().map(|c| child(subst, c, &mut changed));
            changed.then(|| Tuple(na, nb, nc))
        }
        Type(home, name, args) => {
            let mut changed = false;
            let new: Vec<can::Type> = args
                .iter()
                .map(|a| match subst_shared(subst, a) {
                    Some(t) => {
                        changed = true;
                        t
                    }
                    None => a.clone(),
                })
                .collect();
            changed.then(|| Type(home.clone(), name.clone(), Rc::new(new)))
        }
        Record(fields, ext) => {
            let mut changed = false;
            let mut new_fields: Vec<(Name, can::Type)> = fields
                .iter()
                .map(|(n, t)| match subst_shared(subst, t) {
                    Some(t) => {
                        changed = true;
                        (n.clone(), t)
                    }
                    None => (n.clone(), t.clone()),
                })
                .collect();
            // Expand a bound row variable into the record's remaining fields, so
            // a specialized open record becomes the full closed record its
            // concrete layout requires.
            let mut new_ext = ext.clone();
            if let Some(x) = ext {
                if let Some(Record(extra, extra_ext)) = subst.get(x) {
                    changed = true;
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
            changed.then(|| Record(Rc::new(new_fields), new_ext))
        }
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
