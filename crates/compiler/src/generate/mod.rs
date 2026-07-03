//! Port of `Generate.JavaScript` — compile the canonical AST to JavaScript.
//!
//! Uses the same runtime conventions as Elm's kernel: `F2`/`A2` helpers for
//! curried functions, `{ $: 'Ctor', a: ..., b: ... }` objects for custom
//! types, cons cells for lists, and plain objects for records.

use std::fmt::Write;

use crate::ast::canonical as can;
use crate::data::Name;

pub const RUNTIME: &str = include_str!("runtime.js");

pub fn generate(module: &can::Module) -> String {
    let mut gen = Generator {
        out: String::new(),
        module_name: module.name.clone(),
        temp_counter: 0,
    };

    gen.out.push_str("(function () {\n'use strict';\n\n");
    gen.out.push_str(RUNTIME);
    gen.out.push_str("\n// MODULE ");
    gen.out.push_str(module.name.as_str());
    gen.out.push_str("\n\n");

    for union in &module.unions {
        gen.union(union);
    }

    let mut exports = Vec::new();
    for group in &module.decls {
        match group {
            can::DeclGroup::Value(def) => {
                gen.top_level_def(def);
                exports.push(def.name.value.clone());
            }
            can::DeclGroup::Recursive(defs) => {
                for def in defs {
                    gen.top_level_def(def);
                    exports.push(def.name.value.clone());
                }
            }
        }
    }

    let module_var = mangle_module(&gen.module_name);
    let mut export_fields = String::new();
    for (i, name) in exports.iter().enumerate() {
        if i > 0 {
            export_fields.push_str(", ");
        }
        write!(
            export_fields,
            "'{}': {}${}",
            name,
            module_var,
            sanitize(name)
        )
        .unwrap();
    }
    write!(
        gen.out,
        "\nvar Elm = {{ '{}': {{ {} }} }};\n\
         if (typeof module !== 'undefined') {{ module.exports = Elm; }} else {{ this.Elm = Elm; }}\n\
         }}).call(this);\n",
        module.name, export_fields
    )
    .unwrap();

    gen.out
}

fn mangle_module(name: &Name) -> String {
    format!("${}", name.as_str().replace('.', "$"))
}

/// JavaScript reserved words that are legal Elm identifiers.
fn sanitize(name: &str) -> String {
    match name {
        "arguments" | "await" | "break" | "case" | "catch" | "class" | "const" | "continue"
        | "debugger" | "default" | "delete" | "do" | "else" | "enum" | "eval" | "export"
        | "extends" | "finally" | "for" | "function" | "instanceof" | "new" | "null"
        | "return" | "static" | "super" | "switch" | "this" | "throw" | "try" | "typeof"
        | "var" | "void" | "while" | "with" | "yield" => format!("_{}", name),
        _ => name.to_string(),
    }
}

struct Generator {
    out: String,
    module_name: Name,
    temp_counter: usize,
}

impl Generator {
    fn global(&self, name: &Name) -> String {
        format!("{}${}", mangle_module(&self.module_name), sanitize(name))
    }

    fn fresh_temp(&mut self) -> String {
        self.temp_counter += 1;
        format!("_v{}", self.temp_counter)
    }

    // UNIONS

    fn union(&mut self, union: &can::Union) {
        for ctor in &union.ctors {
            let var = self.global(&ctor.name);
            let arity = ctor.args.len();
            if arity == 0 {
                writeln!(self.out, "var {} = {{ $: '{}' }};", var, ctor.name).unwrap();
            } else {
                let params: Vec<String> = (0..arity).map(field_name).collect();
                let fields: Vec<String> = params
                    .iter()
                    .map(|p| format!("{}: {}", p, p))
                    .collect();
                let body = format!(
                    "function ({}) {{ return {{ $: '{}', {} }}; }}",
                    params.join(", "),
                    ctor.name,
                    fields.join(", ")
                );
                if arity == 1 {
                    writeln!(self.out, "var {} = {};", var, body).unwrap();
                } else {
                    writeln!(self.out, "var {} = F{}({});", var, arity, body).unwrap();
                }
            }
        }
        self.out.push('\n');
    }

    // DEFINITIONS

    fn top_level_def(&mut self, def: &can::Def) {
        let var = self.global(&def.name.value);
        let value = self.def_value(def);
        writeln!(self.out, "var {} = {};", var, value).unwrap();
    }

    /// The JS expression for a definition: a function wrapper when it has
    /// arguments, otherwise its body.
    fn def_value(&mut self, def: &can::Def) -> String {
        if def.args.is_empty() {
            self.expr(&def.body)
        } else {
            self.function(&def.args, &def.body)
        }
    }

    fn function(&mut self, args: &[can::Pattern], body: &can::Expr) -> String {
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
        let body_js = self.expr(body);
        let arity = params.len();
        let inner = format!(
            "function ({}) {{ {}return {}; }}",
            params.join(", "),
            prelude,
            body_js
        );
        if arity == 1 {
            inner
        } else {
            format!("F{}({})", arity, inner)
        }
    }

    // EXPRESSIONS

    fn expr(&mut self, expr: &can::Expr) -> String {
        use can::Expr_::*;
        match &expr.value {
            Chr(c) => js_string(&c.to_string()),
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
            VarTopLevel(name) => self.global(name),
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
            Lambda(args, body) => self.function(args, body),
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
            Let(decls, body) => {
                let mut out = String::from("(function () { ");
                for decl in decls {
                    match decl {
                        can::LetDecl::Def(def) => {
                            let value = self.def_value(def);
                            write!(out, "var {} = {}; ", sanitize(&def.name.value), value)
                                .unwrap();
                        }
                        can::LetDecl::Recursive(defs) => {
                            for def in defs {
                                let value = self.def_value(def);
                                write!(out, "var {} = {}; ", sanitize(&def.name.value), value)
                                    .unwrap();
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
                write!(out, "return {}; }})()", self.expr(body)).unwrap();
                out
            }
            Case(scrutinee, branches) => self.case(scrutinee, branches),
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
            _ if *home == self.module_name => self.global(&ctor.name),
            _ => foreign(home, &ctor.name),
        }
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
        match op.as_str() {
            "+" => format!("({} + {})", l, r),
            "-" => format!("({} - {})", l, r),
            "*" => format!("({} * {})", l, r),
            "/" => format!("({} / {})", l, r),
            "//" => format!("(({} / {}) | 0)", l, r),
            "^" => format!("Math.pow({}, {})", l, r),
            "==" => format!("_Utils_eq({}, {})", l, r),
            "/=" => format!("(!_Utils_eq({}, {}))", l, r),
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

    // CASE EXPRESSIONS

    fn case(&mut self, scrutinee: &can::Expr, branches: &[(can::Pattern, can::Expr)]) -> String {
        let temp = self.fresh_temp();
        let mut out = format!(
            "(function () {{ var {} = {}; ",
            temp,
            self.expr(scrutinee)
        );
        for (pattern, branch) in branches {
            let mut tests = Vec::new();
            let mut bindings = Vec::new();
            pattern_tests(pattern, &temp, &mut tests, &mut bindings);
            let mut body = String::new();
            for (name, path) in bindings {
                write!(body, "var {} = {}; ", sanitize(&name), path).unwrap();
            }
            write!(body, "return {};", self.expr(branch)).unwrap();
            if tests.is_empty() {
                write!(out, "{} }})()", body).unwrap();
                return out;
            }
            write!(out, "if ({}) {{ {} }} ", tests.join(" && "), body).unwrap();
        }
        out.push_str("throw new Error('Missing case branch (this Elm code has a non-exhaustive pattern match)'); })()");
        out
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
        Chr(c) => tests.push(format!("{} === {}", path, js_string(&c.to_string()))),
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
