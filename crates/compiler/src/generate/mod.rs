//! Port of `Generate.JavaScript` — compile the canonical AST to JavaScript.
//!
//! Uses the same runtime conventions as Elm's kernel: `F2`/`A2` helpers for
//! curried functions, `{ $: 'Ctor', a: ..., b: ... }` objects for custom
//! types, cons cells for lists, and plain objects for records.

pub mod native;
pub mod typed;

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::ast::canonical as can;
use crate::data::Name;
use crate::reporting::Region;

pub const RUNTIME: &str = include_str!("runtime.js");

/// The runtime source to embed. `ALM_RUNTIME_JS` overrides the compiled-in
/// kernel — used by the mutation test harness to inject mutated runtimes
/// without rebuilding the compiler.
fn runtime_source() -> String {
    match std::env::var("ALM_RUNTIME_JS") {
        Ok(path) => std::fs::read_to_string(path).expect("ALM_RUNTIME_JS must be readable"),
        Err(_) => RUNTIME.to_string(),
    }
}

pub fn generate(module: &can::Module) -> String {
    generate_project(std::slice::from_ref(module))
}

/// Generate a single JavaScript file from all the modules of a project,
/// given in dependency order (dependencies first).
pub fn generate_project(modules: &[can::Module]) -> String {
    generate_project_typed(modules, HashMap::new())
}

/// Like `generate_project`, but with per-module expression types so comparison
/// operators can inline to native JS operators on scalar comparables.
pub fn generate_project_typed(
    modules: &[can::Module],
    mut node_types: HashMap<Name, HashMap<Region, can::Type>>,
) -> String {
    let mut gen = Generator {
        out: String::new(),
        module_name: None,
        temp_counter: 0,
        node_types: HashMap::new(),
        cyclic_values: HashSet::new(),
    };

    gen.out.push_str("(function () {\n'use strict';\n\n");
    gen.out.push_str(&runtime_source());
    gen.out.push_str("\n// HIGHER-ARITY CURRY HELPERS\n");
    for n in 8..=64 {
        let params: Vec<String> = (0..n).map(|i| format!("v{}", i)).collect();
        writeln!(
            gen.out,
            "var F{} = function (fun) {{ return _Fn({}, fun); }};",
            n, n
        )
        .unwrap();
        writeln!(
            gen.out,
            "var A{} = function (f, {}) {{ return _An(f, [{}]); }};",
            n,
            params.join(", "),
            params.join(", ")
        )
        .unwrap();
    }
    gen.out.push_str("\n// BUILTIN UNION CONSTRUCTORS\n");
    for union in crate::builtins::UNIONS {
        // Bool/Order/Maybe/Result constructors are hand-written in the
        // runtime kernel.
        if matches!(union.module, "Basics" | "Maybe" | "Result") {
            continue;
        }
        let module_var = mangle_module(&Name::from(union.module));
        for (ctor_name, args) in union.ctors {
            emit_ctor(&mut gen.out, &module_var, ctor_name, args.len());
        }
    }
    gen.out.push_str("\n// HTML HELPERS (generated from the builtin tables)\n");
    for tag in crate::builtins::HTML_TAGS {
        let dom_tag = tag.trim_end_matches('_');
        writeln!(
            gen.out,
            "var $Html${} = _VDom_node('{}');",
            sanitize(tag),
            dom_tag
        )
        .unwrap();
    }
    for attr in crate::builtins::HTML_STRING_ATTRS {
        if *attr == "value" {
            writeln!(gen.out, "var $Html$Attributes$value = _VDom_prop('value');").unwrap();
        } else {
            let key = match *attr {
                "httpEquiv" => "http-equiv",
                "acceptCharset" => "accept-charset",
                other => other.trim_end_matches('_'),
            };
            writeln!(
                gen.out,
                "var $Html$Attributes${} = function (v) {{ return {{ $: 'AAttr', key: '{}', val: v }}; }};",
                sanitize(attr),
                key
            )
            .unwrap();
        }
    }
    for attr in crate::builtins::HTML_BOOL_ATTRS {
        let property = match *attr {
            "readonly" => "readOnly",
            "novalidate" => "noValidate",
            other => other,
        };
        writeln!(
            gen.out,
            "var $Html$Attributes${} = _VDom_prop('{}');",
            sanitize(attr),
            property
        )
        .unwrap();
    }
    for attr in crate::builtins::HTML_INT_ATTRS {
        writeln!(
            gen.out,
            "var $Html$Attributes${} = function (n) {{ return {{ $: 'AAttr', key: '{}', val: String(n) }}; }};",
            sanitize(attr),
            attr
        )
        .unwrap();
    }
    writeln!(
        gen.out,
        "var $Html$Attributes$classList = function (pairs) {{ var names = []; for (var xs = pairs; xs.$ === '::'; xs = xs.b) {{ if (xs.a.b) {{ names.push(xs.a.a); }} }} return {{ $: 'AAttr', key: 'class', val: names.join(' ') }}; }};"
    )
    .unwrap();
    writeln!(
        gen.out,
        "var $Html$Attributes$property = F2(function (key, value) {{ return {{ $: 'AProp', key: key, val: value }}; }});"
    )
    .unwrap();
    for tag in crate::builtins::SVG_TAGS {
        let dom_tag = tag.trim_end_matches('_');
        writeln!(
            gen.out,
            "var $Svg${} = _VDom_nodeNS('{}');",
            sanitize(tag),
            dom_tag
        )
        .unwrap();
    }
    for (attr, dom_name) in crate::builtins::SVG_ATTRS {
        writeln!(
            gen.out,
            "var $Svg$Attributes${} = function (v) {{ return {{ $: 'AAttr', key: '{}', val: v }}; }};",
            sanitize(attr),
            dom_name
        )
        .unwrap();
    }

    let mut all_exports: Vec<(Name, Vec<Name>)> = Vec::new();
    for module in modules {
        gen.module_name = Some(module.name.clone());
        gen.node_types = node_types.remove(&module.name).unwrap_or_default();
        gen.out.push_str("\n// MODULE ");
        gen.out.push_str(module.name.as_str());
        gen.out.push_str("\n\n");

        for union in &module.unions {
            gen.union(union);
        }
        for port in &module.ports {
            gen.port_decl(port);
        }

        let mut exports = Vec::new();
        for group in &module.decls {
            match group {
                can::DeclGroup::Value(def) => {
                    gen.top_level_def(def);
                    exports.push(def.name.value.clone());
                }
                can::DeclGroup::Recursive(defs) => {
                    gen.recursive_group(defs);
                    for def in defs {
                        exports.push(def.name.value.clone());
                    }
                }
            }
        }
        all_exports.push((module.name.clone(), exports));
    }

    let mut module_objects = String::new();
    for (i, (module_name, exports)) in all_exports.iter().enumerate() {
        if i > 0 {
            module_objects.push_str(", ");
        }
        let module_var = mangle_module(module_name);
        let mut export_fields = String::new();
        for (j, name) in exports.iter().enumerate() {
            if j > 0 {
                export_fields.push_str(", ");
            }
            write!(
                export_fields,
                "'{}': _Platform_wrap({}${})",
                name,
                module_var,
                sanitize(name)
            )
            .unwrap();
        }
        write!(
            module_objects,
            "'{}': {{ {} }}",
            module_name, export_fields
        )
        .unwrap();
    }
    write!(
        gen.out,
        "\nvar Elm = {{ {} }};\n\
         if (typeof module !== 'undefined') {{ module.exports = Elm; }} else {{ this.Elm = Elm; }}\n\
         }}).call(this);\n",
        module_objects
    )
    .unwrap();

    gen.out
}

fn mangle_module(name: &Name) -> String {
    format!("${}", name.as_str().replace('.', "$"))
}

/// JavaScript reserved words that are legal Elm identifiers.
fn sanitize(name: &str) -> String {
    // Mirrors elm's list (Generate.JavaScript.Name.reservedWords). Includes the
    // strict-mode "future reserved words" (interface/implements/package/private/
    // protected/public) and the literals false/true/null — all legal lowercase
    // Elm identifiers but reserved in the strict-mode bundle we emit. elm mangles
    // these the same way: prefix with `_`.
    match name {
        "arguments" | "await" | "break" | "case" | "catch" | "class" | "const" | "continue"
        | "debugger" | "default" | "delete" | "do" | "else" | "enum" | "eval" | "export"
        | "extends" | "false" | "finally" | "for" | "function" | "if" | "implements" | "import"
        | "in" | "instanceof" | "interface" | "let" | "new" | "null" | "package" | "private"
        | "protected" | "public" | "return" | "static" | "super" | "switch" | "this" | "throw"
        | "true" | "try" | "typeof" | "var" | "void" | "while" | "with" | "yield" => {
            format!("_{}", name)
        }
        _ => name.to_string(),
    }
}

struct Generator {
    out: String,
    /// The module whose declarations are being emitted; set before any
    /// definition is generated.
    module_name: Option<Name>,
    temp_counter: usize,
    /// Inferred type of every expression in the current module, keyed by
    /// region. Lets comparison operators inline to native JS `<` etc. when
    /// the operands are scalar comparables (the common, hot case). Empty when
    /// types are unavailable — then comparisons fall back to `_Utils_cmp`.
    node_types: HashMap<Region, can::Type>,
    /// Names of the *value* members of the cyclic top-level group currently
    /// being emitted. References to these are compiled to lazy thunk calls
    /// (`$Module$cyclic$x()`) because the value may not be initialized yet.
    /// Empty except while emitting the bodies of such a group.
    cyclic_values: HashSet<Name>,
}

impl Generator {
    fn module_name(&self) -> &Name {
        self.module_name
            .as_ref()
            .expect("module context is set before emitting declarations")
    }

    fn global(&self, name: &Name) -> String {
        format!("{}${}", mangle_module(self.module_name()), sanitize(name))
    }

    /// The name of the lazy thunk for a cyclic value: `$Module$cyclic$name`.
    fn cyclic_global(&self, name: &Name) -> String {
        format!(
            "{}$cyclic${}",
            mangle_module(self.module_name()),
            sanitize(name)
        )
    }

    fn fresh_temp(&mut self) -> String {
        self.temp_counter += 1;
        format!("_v{}", self.temp_counter)
    }

    // UNIONS

    fn union(&mut self, union: &can::Union) {
        let module_var = mangle_module(self.module_name());
        for ctor in &union.ctors {
            emit_ctor(&mut self.out, &module_var, ctor.name.as_str(), ctor.args.len());
        }
        self.out.push('\n');
    }

    // PORTS

    fn port_decl(&mut self, port: &can::PortDecl) {
        let var = self.global(&port.name);
        match &port.tipe {
            // Outgoing: `name : payload -> Cmd msg`
            can::Type::Lambda(payload, result)
                if matches!(&**result, can::Type::Type(_, n, _) if n.as_str() == "Cmd") =>
            {
                writeln!(
                    self.out,
                    "var {} = _Platform_outgoingPort('{}', {});",
                    var,
                    port.name,
                    to_js_converter(payload)
                )
                .unwrap();
            }
            // Incoming: `name : (payload -> msg) -> Sub msg`
            can::Type::Lambda(handler, result)
                if matches!(&**result, can::Type::Type(_, n, _) if n.as_str() == "Sub") =>
            {
                let payload = match &**handler {
                    can::Type::Lambda(payload, _) => from_js_converter(payload),
                    _ => "function (v) { return v; }".to_string(),
                };
                writeln!(
                    self.out,
                    "var {} = _Platform_incomingPort('{}', {});",
                    var, port.name, payload
                )
                .unwrap();
            }
            _ => {
                // The type checker enforces port shapes are one of the two
                // above; anything else would be an alm bug.
                writeln!(
                    self.out,
                    "var {} = function () {{ throw new Error('bad port {}'); }};",
                    var, port.name
                )
                .unwrap();
            }
        }
    }

    // DEFINITIONS

    fn top_level_def(&mut self, def: &can::Def) {
        let var = self.global(&def.name.value);
        let value = self.def_value(def, SelfRef::TopLevel);
        writeln!(self.out, "var {} = {};", var, value).unwrap();
    }

    /// Emit a group of mutually recursive top-level definitions.
    ///
    /// Function members never run at initialization, so they are emitted as
    /// ordinary `var f = function ...` bindings. Value members would run
    /// eagerly, so a legal cycle among them is broken exactly as Elm does:
    /// each value becomes a lazy thunk `function $M$cyclic$x() { return ...; }`
    /// whose references to sibling values are thunk calls; the values are then
    /// forced in order inside a `try`, and each thunk is replaced by one that
    /// returns the now-computed value (memoization). A genuine infinite
    /// recursion surfaces as the caught stack overflow.
    fn recursive_group(&mut self, defs: &[can::Def]) {
        let is_function = |def: &can::Def| {
            !def.args.is_empty() || matches!(def.body.value, can::Expr_::Lambda(..))
        };
        let values: Vec<&can::Def> = defs.iter().filter(|d| !is_function(d)).collect();

        // Purely mutually-recursive functions need no special handling: their
        // bodies are deferred, so emission order is irrelevant.
        if values.is_empty() {
            for def in defs {
                self.top_level_def(def);
            }
            return;
        }

        self.cyclic_values = values.iter().map(|d| d.name.value.clone()).collect();

        // Function members first: a value's thunk may call them while forcing.
        for def in defs {
            if is_function(def) {
                self.top_level_def(def);
            }
        }
        // Lazy thunks for the value members.
        for def in &values {
            let thunk = self.cyclic_global(&def.name.value);
            let body = self.expr(&def.body);
            writeln!(self.out, "function {}() {{ return {}; }}", thunk, body).unwrap();
        }
        // Force the values in order, memoizing each thunk once computed.
        self.out.push_str("try {\n");
        for def in &values {
            let var = self.global(&def.name.value);
            let thunk = self.cyclic_global(&def.name.value);
            writeln!(self.out, "  var {} = {}();", var, thunk).unwrap();
            writeln!(self.out, "  {} = function () {{ return {}; }};", thunk, var).unwrap();
        }
        let names = values
            .iter()
            .map(|d| d.name.value.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let module = self.module_name().clone();
        writeln!(
            self.out,
            "}} catch ($) {{ throw new Error('Some top-level definitions from `{}` are causing infinite recursion: {}'); }}",
            module,
            names
        )
        .unwrap();

        self.cyclic_values.clear();
    }

    /// The JS expression for a definition: a function wrapper when it has
    /// arguments, otherwise its body.
    fn def_value(&mut self, def: &can::Def, self_ref: SelfRef) -> String {
        if !def.args.is_empty() {
            return self.function_named(Some((&def.name.value, self_ref)), &def.args, &def.body);
        }
        // `f = \a b -> ...` still gets tail-call optimization.
        if let can::Expr_::Lambda(args, body) = &def.body.value {
            return self.function_named(Some((&def.name.value, self_ref)), args, body);
        }
        self.expr(&def.body)
    }

    fn function(&mut self, args: &[can::Pattern], body: &can::Expr) -> String {
        self.function_named(None, args, body)
    }

    /// Generate a function. When `self_ref` is given and the body contains
    /// tail calls to itself, compile the recursion into a `while` loop —
    /// the port of Elm's TailDef optimization.
    fn function_named(
        &mut self,
        self_ref: Option<(&Name, SelfRef)>,
        args: &[can::Pattern],
        body: &can::Expr,
    ) -> String {
        let mut params = Vec::new();
        let mut prelude = String::new();
        for arg in args {
            match &arg.value {
                can::Pattern_::Var(name) => params.push(sanitize(name)),
                _ => {
                    let temp = self.fresh_temp();
                    let mut bindings = Vec::new();
                    destructure(arg, &temp, &mut bindings);
                    for (name, path) in bindings {
                        write!(prelude, "var {} = {}; ", sanitize(&name), path).unwrap();
                    }
                    params.push(temp);
                }
            }
        }
        let arity = params.len();

        let is_tail_recursive = self_ref
            .as_ref()
            .is_some_and(|(name, self_ref)| {
                has_self_tail_call(name, *self_ref, arity, body)
            });

        let body_js = if is_tail_recursive {
            let (name, self_kind) = self_ref.unwrap();
            let tail = Tail::Loop {
                name: name.clone(),
                self_kind,
                params: params.clone(),
            };
            let inner = self.stmts(body, &tail);
            format!("while (true) {{ {}{} }}", prelude, inner)
        } else {
            format!("{}{}", prelude, self.stmts(body, &Tail::Return))
        };

        let inner = format!("function ({}) {{ {} }}", params.join(", "), body_js);
        if arity == 1 {
            inner
        } else {
            format!("F{}({})", arity, inner)
        }
    }

    // STATEMENTS — function bodies are statements so that `if`, `case`,
    // and `let` in tail position produce plain returns (and tail recursion
    // can `continue` the surrounding loop).

    fn stmts(&mut self, expr: &can::Expr, tail: &Tail) -> String {
        use can::Expr_::*;
        match &expr.value {
            If(branches, otherwise) => {
                let mut out = String::new();
                for (condition, branch) in branches {
                    write!(
                        out,
                        "if ({}) {{ {} }} else ",
                        self.expr(condition),
                        self.stmts(branch, tail)
                    )
                    .unwrap();
                }
                write!(out, "{{ {} }}", self.stmts(otherwise, tail)).unwrap();
                out
            }
            Let(decls, body) => {
                let mut out = String::new();
                for decl in decls {
                    self.let_decl_stmts(decl, &mut out);
                }
                out.push_str(&self.stmts(body, tail));
                out
            }
            Case(scrutinee, branches) => {
                let temp = self.fresh_temp();
                let mut out = format!("var {} = {}; ", temp, self.expr(scrutinee));
                for (pattern, branch) in branches {
                    let mut tests = Vec::new();
                    let mut bindings = Vec::new();
                    pattern_tests(pattern, &temp, &mut tests, &mut bindings);
                    let mut body = String::new();
                    for (name, path) in bindings {
                        write!(body, "var {} = {}; ", sanitize(&name), path).unwrap();
                    }
                    body.push_str(&self.stmts(branch, tail));
                    if tests.is_empty() {
                        out.push_str(&body);
                        return out;
                    }
                    write!(out, "if ({}) {{ {} }} ", tests.join(" && "), body).unwrap();
                }
                out.push_str(
                    "throw new Error('Missing case branch (compiler bug: exhaustiveness checking should have caught this)');",
                );
                out
            }
            Call(func, call_args) => {
                if let Tail::Loop {
                    name,
                    self_kind,
                    params,
                } = tail
                {
                    if is_self_ref(func, name, *self_kind) && call_args.len() == params.len() {
                        // Compute all new arguments before reassigning.
                        let mut out = String::new();
                        let temps: Vec<String> = call_args
                            .iter()
                            .map(|arg| {
                                let temp = self.fresh_temp();
                                write!(out, "var {} = {}; ", temp, self.expr(arg)).unwrap();
                                temp
                            })
                            .collect();
                        for (param, temp) in params.iter().zip(temps) {
                            write!(out, "{} = {}; ", param, temp).unwrap();
                        }
                        out.push_str("continue;");
                        return out;
                    }
                }
                format!("return {};", self.expr(expr))
            }
            _ => format!("return {};", self.expr(expr)),
        }
    }

    fn let_decl_stmts(&mut self, decl: &can::LetDecl, out: &mut String) {
        match decl {
            can::LetDecl::Def(def) => {
                let value = self.def_value(def, SelfRef::Local);
                write!(out, "var {} = {}; ", sanitize(&def.name.value), value).unwrap();
            }
            can::LetDecl::Recursive(defs) => {
                for def in defs {
                    let value = self.def_value(def, SelfRef::Local);
                    write!(out, "var {} = {}; ", sanitize(&def.name.value), value).unwrap();
                }
            }
            can::LetDecl::Destruct(pattern, value) => {
                let temp = self.fresh_temp();
                write!(out, "var {} = {}; ", temp, self.expr(value)).unwrap();
                let mut bindings = Vec::new();
                destructure(pattern, &temp, &mut bindings);
                for (name, path) in bindings {
                    write!(out, "var {} = {}; ", sanitize(&name), path).unwrap();
                }
            }
        }
    }

    // EXPRESSIONS

    fn expr(&mut self, expr: &can::Expr) -> String {
        use can::Expr_::*;
        match &expr.value {
            Chr(c) => format!("_Utils_chr({})", js_string(&c.to_string())),
            Str(s) => js_string(s),
            Int(n) => n.to_string(),
            Float(f) => {
                let s = f.to_string();
                if s.contains('.') || s.contains('e') || s.contains("Infinity") {
                    s
                } else {
                    format!("{}.0", s)
                }
            }
            VarLocal(name) => sanitize(name),
            VarTopLevel(name) => {
                // Inside a value cycle, a reference to another value member is
                // a call to its lazy thunk so it is forced on demand.
                if self.cyclic_values.contains(name) {
                    format!("{}()", self.cyclic_global(name))
                } else {
                    self.global(name)
                }
            }
            VarForeign(module, name) => foreign(module, name),
            VarCtor(home, _union, ctor) => self.ctor_ref(home, ctor),
            List(items) => {
                if items.is_empty() {
                    "_List_Nil".to_string()
                } else {
                    let rendered: Vec<String> = items.iter().map(|e| self.expr(e)).collect();
                    format!("_List_fromArray([{}])", rendered.join(", "))
                }
            }
            Negate(inner) => format!("-({})", self.expr(inner)),
            Binop(op, home, function, left, right) => {
                self.binop(op, home, function, left, right)
            }
            Lambda(args, body) => match record_ctor_fields(args, body) {
                // A record type-alias constructor used as a value: emit a shared,
                // memoized constructor so `(==)` matches elm (see _Record_ctor).
                Some(fields) => format!("_Record_ctor('{}')", fields),
                None => self.function(args, body),
            },
            Call(func, args) => {
                let func_js = self.expr(func);
                let arg_js: Vec<String> = args.iter().map(|a| self.expr(a)).collect();
                match arg_js.len() {
                    1 => format!("{}({})", callable(func_js), arg_js[0]),
                    n => format!("A{}({}, {})", n, func_js, arg_js.join(", ")),
                }
            }
            If(branches, otherwise) => {
                let mut out = String::new();
                for (condition, branch) in branches {
                    write!(
                        out,
                        "({} ? {} : ",
                        self.expr(condition),
                        self.expr(branch)
                    )
                    .unwrap();
                }
                write!(out, "{}{}", self.expr(otherwise), ")".repeat(branches.len()))
                    .unwrap();
                out
            }
            Let(..) | Case(..) => {
                format!("(function () {{ {} }})()", self.stmts(expr, &Tail::Return))
            }
            Accessor(field) => format!("function ($) {{ return $.{}; }}", field),
            Access(record, field) => format!("{}.{}", self.expr(record), field.value),
            Update(record, fields) => {
                let rendered: Vec<String> = fields
                    .iter()
                    .map(|(field, value)| format!("{}: {}", field.value, self.expr(value)))
                    .collect();
                format!(
                    "_Utils_update({}, {{ {} }})",
                    self.expr(record),
                    rendered.join(", ")
                )
            }
            Record(fields) => {
                let rendered: Vec<String> = fields
                    .iter()
                    .map(|(field, value)| format!("{}: {}", field.value, self.expr(value)))
                    .collect();
                format!("{{ {} }}", rendered.join(", "))
            }
            Unit => "_Utils_Tuple0".to_string(),
            Tuple(a, b, rest) => match rest.first() {
                None => format!(
                    "{{ $: '#2', a: {}, b: {} }}",
                    self.expr(a),
                    self.expr(b)
                ),
                Some(c) => format!(
                    "{{ $: '#3', a: {}, b: {}, c: {} }}",
                    self.expr(a),
                    self.expr(b),
                    self.expr(c)
                ),
            },
        }
    }

    fn ctor_ref(&mut self, home: &Name, ctor: &can::Ctor) -> String {
        match (home.as_str(), ctor.name.as_str()) {
            ("Basics", "True") => "true".to_string(),
            ("Basics", "False") => "false".to_string(),
            _ if home == self.module_name() => self.global(&ctor.name),
            _ => foreign(home, &ctor.name),
        }
    }

    /// Whether a comparison whose left operand is `left` can use native JS
    /// comparison operators. True only when the operand's inferred type is a
    /// scalar comparable (Int/Float/Char/String); native `<` on those matches
    /// `_Utils_cmp`, but on lists/tuples it does not, so those must stay on the
    /// kernel. Conservative: unknown/absent type => false.
    fn is_scalar_comparison(&self, left: &can::Expr) -> bool {
        matches!(
            self.node_types.get(&left.region),
            Some(can::Type::Type(module, name, args))
                if args.is_empty()
                    && matches!(
                        (module.as_str(), name.as_str()),
                        ("Basics", "Int")
                            | ("Basics", "Float")
                            | ("Char", "Char")
                            | ("String", "String")
                    )
        )
    }

    /// Inline the hot-path operators exactly like Generate/JavaScript does;
    /// fall back to the kernel functions otherwise.
    fn binop(
        &mut self,
        op: &Name,
        home: &Name,
        function: &Name,
        left: &can::Expr,
        right: &can::Expr,
    ) -> String {
        let l = self.expr(left);
        let r = self.expr(right);
        // Inline `<`, `<=`, `>`, `>=` to native JS operators when the operands
        // are scalar comparables (Int/Float/Char/String) — the hot case. For
        // lists/tuples (or unknown types) fall back to `_Utils_cmp`, which is
        // the only correct choice there. Matches Elm's --optimize codegen.
        let scalar = self.is_scalar_comparison(left);
        match op.as_str() {
            "+" => format!("({} + {})", l, r),
            "-" => format!("({} - {})", l, r),
            "*" => format!("({} * {})", l, r),
            "/" => format!("({} / {})", l, r),
            "//" => format!("(({} / {}) | 0)", l, r),
            "^" => format!("Math.pow({}, {})", l, r),
            "==" => format!("_Utils_eq({}, {})", l, r),
            "/=" => format!("(!_Utils_eq({}, {}))", l, r),
            "<" if scalar => format!("({} < {})", l, r),
            ">" if scalar => format!("({} > {})", l, r),
            "<=" if scalar => format!("({} <= {})", l, r),
            ">=" if scalar => format!("({} >= {})", l, r),
            "<" => format!("(_Utils_cmp({}, {}) < 0)", l, r),
            ">" => format!("(_Utils_cmp({}, {}) > 0)", l, r),
            "<=" => format!("(_Utils_cmp({}, {}) < 1)", l, r),
            ">=" => format!("(_Utils_cmp({}, {}) > -1)", l, r),
            "&&" => format!("({} && {})", l, r),
            "||" => format!("({} || {})", l, r),
            "++" => format!("_Utils_ap({}, {})", l, r),
            "::" => format!("_List_Cons({}, {})", l, r),
            "|>" => format!("{}({})", callable(r), l),
            "<|" => format!("{}({})", callable(l), r),
            _ => format!("A2({}, {}, {})", foreign(home, function), l, r),
        }
    }

}

/// How a definition refers to itself in its own body.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelfRef {
    TopLevel,
    Local,
}

enum Tail {
    Return,
    Loop {
        name: Name,
        self_kind: SelfRef,
        params: Vec<String>,
    },
}

fn is_self_ref(expr: &can::Expr, name: &Name, self_kind: SelfRef) -> bool {
    match (&expr.value, self_kind) {
        (can::Expr_::VarTopLevel(n), SelfRef::TopLevel) => n == name,
        (can::Expr_::VarLocal(n), SelfRef::Local) => n == name,
        _ => false,
    }
}

/// Does the body contain a call to itself in tail position?
fn has_self_tail_call(name: &Name, self_kind: SelfRef, arity: usize, body: &can::Expr) -> bool {
    use can::Expr_::*;
    match &body.value {
        Call(func, args) => is_self_ref(func, name, self_kind) && args.len() == arity,
        If(branches, otherwise) => {
            branches
                .iter()
                .any(|(_, b)| has_self_tail_call(name, self_kind, arity, b))
                || has_self_tail_call(name, self_kind, arity, otherwise)
        }
        Let(decls, inner) => {
            // A shadowing let definition would capture the name.
            let shadowed = decls.iter().any(|decl| match decl {
                can::LetDecl::Def(def) => def.name.value == *name,
                can::LetDecl::Recursive(defs) => defs.iter().any(|d| d.name.value == *name),
                can::LetDecl::Destruct(..) => false,
            });
            !shadowed && has_self_tail_call(name, self_kind, arity, inner)
        }
        Case(_, branches) => branches
            .iter()
            .any(|(_, b)| has_self_tail_call(name, self_kind, arity, b)),
        _ => false,
    }
}

/// Wrap a generated function expression in parens when required so it can
/// be called directly.
fn callable(js: String) -> String {
    if js.starts_with("function") {
        format!("({})", js)
    } else {
        js
    }
}

fn foreign(module: &Name, name: &Name) -> String {
    format!("${}${}", module.as_str().replace('.', "$"), sanitize(name))
}

/// Recognize a record type-alias constructor lambda as produced by
/// `record_alias_ctor` in canonicalization: args are `_r0.._r{n-1}` in order and
/// the body is a record whose i-th field's value is exactly `_r{i}`. Returns the
/// comma-joined field names, so codegen can emit a shared memoized constructor
/// (so equality of records built from the constructor matches elm's semantics).
fn record_ctor_fields(args: &[can::Pattern], body: &can::Expr) -> Option<String> {
    let n = args.len();
    if n == 0 {
        return None;
    }
    for (i, arg) in args.iter().enumerate() {
        match &arg.value {
            can::Pattern_::Var(name) if name.as_str() == format!("_r{}", i) => {}
            _ => return None,
        }
    }
    let fields = match &body.value {
        can::Expr_::Record(fields) => fields,
        _ => return None,
    };
    if fields.len() != n {
        return None;
    }
    let mut names = Vec::with_capacity(n);
    for (i, (fname, fexpr)) in fields.iter().enumerate() {
        match &fexpr.value {
            can::Expr_::VarLocal(vn) if vn.as_str() == format!("_r{}", i) => {}
            _ => return None,
        }
        names.push(fname.value.as_str().to_string());
    }
    Some(names.join(","))
}

/// Emit one union constructor: a value for arity 0, otherwise a curried
/// function building the tagged object.
fn emit_ctor(out: &mut String, module_var: &str, ctor_name: &str, arity: usize) {
    let var = format!("{}${}", module_var, sanitize(ctor_name));
    if arity == 0 {
        writeln!(out, "var {} = {{ $: '{}' }};", var, ctor_name).unwrap();
        return;
    }
    let params: Vec<String> = (0..arity).map(field_name).collect();
    let fields: Vec<String> = params.iter().map(|p| format!("{}: {}", p, p)).collect();
    let body = format!(
        "function ({}) {{ return {{ $: '{}', {} }}; }}",
        params.join(", "),
        ctor_name,
        fields.join(", ")
    );
    if arity == 1 {
        writeln!(out, "var {} = {};", var, body).unwrap();
    } else {
        writeln!(out, "var {} = F{}({});", var, arity, body).unwrap();
    }
}

fn field_name(index: usize) -> String {
    // a, b, ..., z, a1, b1, ...
    let letter = (b'a' + (index % 26) as u8) as char;
    if index < 26 {
        letter.to_string()
    } else {
        format!("{}{}", letter, index / 26)
    }
}

/// Compute variable bindings for an irrefutable pattern (function args and
/// destructuring lets).
fn destructure(pattern: &can::Pattern, path: &str, bindings: &mut Vec<(String, String)>) {
    let mut tests = Vec::new();
    pattern_tests(pattern, path, &mut tests, bindings);
    // Irrefutable patterns generate no tests (the type checker has made
    // sure of it, except for single-ctor unions which always match).
}

/// Compute the tests and bindings for matching `pattern` against `path`.
fn pattern_tests(
    pattern: &can::Pattern,
    path: &str,
    tests: &mut Vec<String>,
    bindings: &mut Vec<(String, String)>,
) {
    use can::Pattern_::*;
    match &pattern.value {
        Anything | Unit => {}
        Var(name) => bindings.push((name.to_string(), path.to_string())),
        Alias(inner, name) => {
            bindings.push((name.value.to_string(), path.to_string()));
            pattern_tests(inner, path, tests, bindings);
        }
        // A Char scrutinee is a boxed `new String(c)`; unwrap it to compare
        // against the primitive char literal (`new String('a') === "a"` is false).
        Chr(c) => tests.push(format!("{}.valueOf() === {}", path, js_string(&c.to_string()))),
        Str(s) => tests.push(format!("{} === {}", path, js_string(s))),
        Int(n) => tests.push(format!("{} === {}", path, n)),
        Record(fields) => {
            for field in fields {
                bindings.push((
                    field.value.to_string(),
                    format!("{}.{}", path, field.value),
                ));
            }
        }
        Tuple(a, b, rest) => {
            pattern_tests(a, &format!("{}.a", path), tests, bindings);
            pattern_tests(b, &format!("{}.b", path), tests, bindings);
            if let Some(c) = rest.first() {
                pattern_tests(c, &format!("{}.c", path), tests, bindings);
            }
        }
        Ctor(home, _union, ctor, args) => {
            match (home.as_str(), ctor.name.as_str()) {
                ("Basics", "True") => tests.push(format!("{} === true", path)),
                ("Basics", "False") => tests.push(format!("{} === false", path)),
                _ => {
                    if ctor.num_ctors > 1 {
                        tests.push(format!("{}.$ === '{}'", path, ctor.name));
                    }
                }
            }
            for (i, arg) in args.iter().enumerate() {
                pattern_tests(arg, &format!("{}.{}", path, field_name(i)), tests, bindings);
            }
        }
        List(items) => {
            let mut current = path.to_string();
            for item in items {
                tests.push(format!("{}.$ === '::'", current));
                pattern_tests(item, &format!("{}.a", current), tests, bindings);
                current = format!("{}.b", current);
            }
            tests.push(format!("{}.$ === '[]'", current));
        }
        Cons(head, tail) => {
            tests.push(format!("{}.$ === '::'", path));
            pattern_tests(head, &format!("{}.a", path), tests, bindings);
            pattern_tests(tail, &format!("{}.b", path), tests, bindings);
        }
    }
}

// PORT CONVERTERS — JS expressions converting between Elm values and the
// plain JS values that flow through ports, driven by the port's type.

fn to_js_converter(tipe: &can::Type) -> String {
    use can::Type::*;
    match tipe {
        Type(_, name, args) => match name.as_str() {
            "Int" | "Float" | "Bool" | "String" | "Char" | "Value" => "_Port_id".to_string(),
            "List" => format!(
                "function (l) {{ return _List_toArray(l).map({}); }}",
                to_js_converter(&args[0])
            ),
            "Array" => format!(
                "function (a) {{ return a.a.map({}); }}",
                to_js_converter(&args[0])
            ),
            "Maybe" => format!(
                "function (m) {{ return m.$ === 'Just' ? ({})(m.a) : null; }}",
                to_js_converter(&args[0])
            ),
            _ => "_Port_id".to_string(),
        },
        Unit => "function (_v) { return null; }".to_string(),
        Record(fields, _) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(name, t)| format!("{}: ({})(r.{})", name, to_js_converter(t), name))
                .collect();
            format!("function (r) {{ return {{ {} }}; }}", parts.join(", "))
        }
        Tuple(a, b, c) => {
            let mut parts = vec![
                format!("({})(t.a)", to_js_converter(a)),
                format!("({})(t.b)", to_js_converter(b)),
            ];
            if let Some(c) = c {
                parts.push(format!("({})(t.c)", to_js_converter(c)));
            }
            format!("function (t) {{ return [{}]; }}", parts.join(", "))
        }
        _ => "_Port_id".to_string(),
    }
}

fn from_js_converter(tipe: &can::Type) -> String {
    use can::Type::*;
    match tipe {
        Type(_, name, args) => match name.as_str() {
            "Int" | "Float" | "Bool" | "String" | "Char" | "Value" => "_Port_id".to_string(),
            "List" => format!(
                "function (a) {{ return _List_fromArray(a.map({})); }}",
                from_js_converter(&args[0])
            ),
            "Array" => format!(
                "function (a) {{ return {{ $: 'Array_elm_builtin', a: a.map({}) }}; }}",
                from_js_converter(&args[0])
            ),
            "Maybe" => format!(
                "function (v) {{ return v === null || v === undefined ? $Maybe$Nothing : $Maybe$Just(({})(v)); }}",
                from_js_converter(&args[0])
            ),
            _ => "_Port_id".to_string(),
        },
        Unit => "function (_v) { return _Utils_Tuple0; }".to_string(),
        Record(fields, _) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(name, t)| format!("{}: ({})(r.{})", name, from_js_converter(t), name))
                .collect();
            format!("function (r) {{ return {{ {} }}; }}", parts.join(", "))
        }
        Tuple(a, b, c) => match c {
            None => format!(
                "function (t) {{ return {{ $: '#2', a: ({})(t[0]), b: ({})(t[1]) }}; }}",
                from_js_converter(a),
                from_js_converter(b)
            ),
            Some(c) => format!(
                "function (t) {{ return {{ $: '#3', a: ({})(t[0]), b: ({})(t[1]), c: ({})(t[2]) }}; }}",
                from_js_converter(a),
                from_js_converter(b),
                from_js_converter(c)
            ),
        },
        _ => "_Port_id".to_string(),
    }
}

fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{{{:x}}}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}
