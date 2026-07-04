//! A lowered, backend-neutral IR sitting between the canonical AST and
//! code generation.
//!
//! The canonical AST is what the JS backend consumes directly; native
//! backends want the hard parts done first. Lowering performs:
//!
//! - **Closure conversion**: every lambda is lifted to a top-level
//!   [`Function`] with an explicit capture list; a lambda occurrence
//!   becomes [`Expr::Closure`].
//! - **Call resolution**: saturated calls to known functions become
//!   [`Expr::CallDirect`]; everything else goes through the generic
//!   [`Expr::CallClosure`] (the runtime's curried apply).
//! - **Pattern compilation**: `case` branches become explicit test lists
//!   over structural paths ([`Branch`]), the same shape the JS backend
//!   emits as `if` chains.
//! - **Tail recursion**: self tail calls are rewritten to
//!   [`Expr::TailCall`] and the function is marked `tail_recursive`, so a
//!   backend can emit a loop.
//!
//! The IR is untyped: values use a uniform boxed representation, exactly
//! like the JS backend's runtime conventions. Threading inferred types
//! through (for unboxing and monomorphization) is a later, separate step.

pub mod lower;

use std::fmt::Write;

use crate::data::Name;

#[derive(Debug, Clone)]
pub struct Program {
    /// All functions: top-level definitions, lifted lambdas, constructor
    /// wrappers, and field accessors.
    pub functions: Vec<Function>,
    /// Zero-argument top-level values, in initialization order.
    pub values: Vec<GlobalValue>,
    /// The entry module's `main`, if it has one.
    pub main: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    /// Capture parameters first, then the source-level arguments.
    pub params: Vec<String>,
    /// How many leading `params` are captures.
    pub captures: usize,
    /// When true the body contains `TailCall`s that re-enter this function
    /// with new (non-capture) arguments; backends should emit a loop.
    pub tail_recursive: bool,
    pub body: Expr,
}

#[derive(Debug, Clone)]
pub struct GlobalValue {
    pub name: String,
    pub body: Expr,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Chr(char),
    Unit,
    /// A local variable: parameter, let binding, or pattern binding.
    Local(String),
    /// Reference to a zero-argument top-level value.
    GlobalValue(String),
    /// Allocate a closure over a function; `captures` fill the function's
    /// leading capture parameters. A top-level function used as a value is
    /// a closure with no captures.
    Closure {
        function: String,
        captures: Vec<Expr>,
    },
    /// Saturated call to a known function. Argument count always equals
    /// the function's parameter count (captures included for lifted
    /// lambdas — callers of `CallDirect` never pass captures, lowering
    /// only emits it for functions whose captures are empty).
    CallDirect {
        function: String,
        args: Vec<Expr>,
    },
    /// Call through a function value; the runtime's generic apply deals
    /// with partial and over-application.
    CallClosure {
        function: Box<Expr>,
        args: Vec<Expr>,
    },
    /// Saturated call to a built-in kernel function via its exported
    /// symbol, bypassing the closure/apply machinery. The symbol is
    /// `available_externally` in the merged runtime bitcode, so the
    /// optimizer can inline it into generated code.
    CallBuiltin {
        symbol: String,
        args: Vec<Expr>,
    },
    /// A value provided by the runtime kernel (built-in modules), e.g.
    /// `String.fromInt`. Its arity is unknown to the compiler.
    Foreign {
        module: Name,
        name: Name,
    },
    /// Operators the backends inline. `And`/`Or` short-circuit: the second
    /// argument must only be evaluated if needed.
    Prim {
        op: PrimOp,
        args: Vec<Expr>,
    },
    /// Construct a custom type value.
    Ctor {
        name: Name,
        index: u32,
        args: Vec<Expr>,
    },
    List(Vec<Expr>),
    /// Two or three elements.
    Tuple(Vec<Expr>),
    Record(Vec<(Name, Expr)>),
    Update {
        record: Box<Expr>,
        fields: Vec<(Name, Expr)>,
    },
    Access {
        record: Box<Expr>,
        field: Name,
    },
    /// Structural access produced by pattern compilation.
    GetField {
        of: Box<Expr>,
        step: Step,
    },
    If {
        branches: Vec<(Expr, Expr)>,
        otherwise: Box<Expr>,
    },
    Let {
        name: String,
        value: Box<Expr>,
        body: Box<Expr>,
    },
    /// Mutually recursive local closures. Every binding is a `Closure`
    /// whose captures may reference the bound names; backends allocate all
    /// closures first and then patch the recursive capture slots.
    LetRec {
        bindings: Vec<(String, Expr)>,
        body: Box<Expr>,
    },
    /// Pattern match: bind the scrutinee to `temp`, then take the first
    /// branch whose tests all pass. Exhaustiveness checking guarantees
    /// some branch matches.
    Case {
        scrutinee: Box<Expr>,
        temp: String,
        branches: Vec<Branch>,
    },
    /// Re-enter the enclosing `tail_recursive` function with new values
    /// for its non-capture parameters.
    TailCall {
        args: Vec<Expr>,
    },
    /// Unreachable (missing case branch — a compiler bug if ever hit).
    Crash(String),
}

#[derive(Debug, Clone)]
pub struct Branch {
    /// All must pass, in order, for the branch to be taken.
    pub tests: Vec<(Expr, Test)>,
    pub bindings: Vec<(String, Expr)>,
    pub body: Expr,
}

#[derive(Debug, Clone)]
pub enum Test {
    IsCtor { name: Name, index: u32 },
    IsBool(bool),
    IsInt(i64),
    IsChr(char),
    IsStr(String),
    IsCons,
    IsNil,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    CtorArg(u32),
    TupleField(u32),
    ListHead,
    ListTail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimOp {
    Add,
    Sub,
    Mul,
    FDiv,
    IDiv,
    Pow,
    Neg,
    Eq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Append,
    Cons,
}

impl PrimOp {
    pub fn name(self) -> &'static str {
        match self {
            PrimOp::Add => "add",
            PrimOp::Sub => "sub",
            PrimOp::Mul => "mul",
            PrimOp::FDiv => "fdiv",
            PrimOp::IDiv => "idiv",
            PrimOp::Pow => "pow",
            PrimOp::Neg => "neg",
            PrimOp::Eq => "eq",
            PrimOp::NotEq => "neq",
            PrimOp::Lt => "lt",
            PrimOp::Le => "le",
            PrimOp::Gt => "gt",
            PrimOp::Ge => "ge",
            PrimOp::And => "and",
            PrimOp::Or => "or",
            PrimOp::Append => "append",
            PrimOp::Cons => "cons",
        }
    }
}

// PRETTY PRINTING — a stable textual form for tests and debugging.

impl Program {
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        for function in &self.functions {
            let mut params = Vec::new();
            for (i, p) in function.params.iter().enumerate() {
                if i < function.captures {
                    params.push(format!("^{}", p));
                } else {
                    params.push(p.clone());
                }
            }
            writeln!(
                out,
                "function{} {}({}):",
                if function.tail_recursive { " (tail)" } else { "" },
                function.name,
                params.join(", ")
            )
            .unwrap();
            print_expr(&mut out, &function.body, 1);
            out.push('\n');
        }
        for value in &self.values {
            writeln!(out, "value {}:", value.name).unwrap();
            print_expr(&mut out, &value.body, 1);
            out.push('\n');
        }
        if let Some(main) = &self.main {
            writeln!(out, "main = {}", main).unwrap();
        }
        out
    }
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn print_expr(out: &mut String, expr: &Expr, level: usize) {
    indent(out, level);
    match expr {
        Expr::Let { name, value, body } => {
            writeln!(out, "let {} =", name).unwrap();
            print_expr(out, value, level + 1);
            indent(out, level);
            out.push_str("in\n");
            print_expr(out, body, level);
        }
        Expr::LetRec { bindings, body } => {
            out.push_str("letrec\n");
            for (name, value) in bindings {
                indent(out, level + 1);
                writeln!(out, "{} = {}", name, inline_expr(value)).unwrap();
            }
            indent(out, level);
            out.push_str("in\n");
            print_expr(out, body, level);
        }
        Expr::If {
            branches,
            otherwise,
        } => {
            for (condition, branch) in branches {
                writeln!(out, "if {} then", inline_expr(condition)).unwrap();
                print_expr(out, branch, level + 1);
                indent(out, level);
                out.push_str("else ");
                out.push('\n');
            }
            print_expr(out, otherwise, level + 1);
        }
        Expr::Case {
            scrutinee,
            temp,
            branches,
        } => {
            writeln!(out, "case {} = {} of", temp, inline_expr(scrutinee)).unwrap();
            for branch in branches {
                indent(out, level + 1);
                let tests: Vec<String> = branch
                    .tests
                    .iter()
                    .map(|(subject, test)| {
                        format!("{} {}", print_test(test), inline_expr(subject))
                    })
                    .collect();
                if tests.is_empty() {
                    out.push_str("otherwise");
                } else {
                    write!(out, "when {}", tests.join(" && ")).unwrap();
                }
                if !branch.bindings.is_empty() {
                    let bindings: Vec<String> = branch
                        .bindings
                        .iter()
                        .map(|(name, path)| format!("{} = {}", name, inline_expr(path)))
                        .collect();
                    write!(out, " [{}]", bindings.join(", ")).unwrap();
                }
                out.push_str(" ->\n");
                print_expr(out, &branch.body, level + 2);
            }
        }
        _ => {
            out.push_str(&inline_expr(expr));
            out.push('\n');
        }
    }
}

fn print_test(test: &Test) -> String {
    match test {
        Test::IsCtor { name, index } => format!("is-ctor[{} #{}]", name, index),
        Test::IsBool(b) => format!("is-{}", b),
        Test::IsInt(n) => format!("is-int[{}]", n),
        Test::IsChr(c) => format!("is-chr[{}]", c),
        Test::IsStr(s) => format!("is-str[{:?}]", s),
        Test::IsCons => "is-cons".to_string(),
        Test::IsNil => "is-nil".to_string(),
    }
}

fn print_step(step: Step) -> String {
    match step {
        Step::CtorArg(i) => format!("arg{}", i),
        Step::TupleField(i) => format!("item{}", i),
        Step::ListHead => "head".to_string(),
        Step::ListTail => "tail".to_string(),
    }
}

fn inline_list(items: &[Expr]) -> String {
    items.iter().map(inline_expr).collect::<Vec<_>>().join(" ")
}

fn inline_expr(expr: &Expr) -> String {
    match expr {
        Expr::Bool(b) => b.to_string(),
        Expr::Int(n) => n.to_string(),
        Expr::Float(f) => format!("{}f", f),
        Expr::Str(s) => format!("{:?}", s),
        Expr::Chr(c) => format!("{:?}", c),
        Expr::Unit => "unit".to_string(),
        Expr::Local(name) => name.clone(),
        Expr::GlobalValue(name) => format!("(global {})", name),
        Expr::Closure { function, captures } => {
            if captures.is_empty() {
                format!("(closure {})", function)
            } else {
                format!("(closure {} [{}])", function, inline_list(captures))
            }
        }
        Expr::CallDirect { function, args } => {
            format!("(call {} {})", function, inline_list(args))
        }
        Expr::CallClosure { function, args } => {
            format!("(apply {} {})", inline_expr(function), inline_list(args))
        }
        Expr::CallBuiltin { symbol, args } => {
            format!("(builtin {} {})", symbol, inline_list(args))
        }
        Expr::Foreign { module, name } => format!("(foreign {}.{})", module, name),
        Expr::Prim { op, args } => format!("({} {})", op.name(), inline_list(args)),
        Expr::Ctor { name, index, args } => {
            if args.is_empty() {
                format!("(ctor {} #{})", name, index)
            } else {
                format!("(ctor {} #{} {})", name, index, inline_list(args))
            }
        }
        Expr::List(items) => format!("(list {})", inline_list(items)),
        Expr::Tuple(items) => format!("(tuple {})", inline_list(items)),
        Expr::Record(fields) => {
            let rendered: Vec<String> = fields
                .iter()
                .map(|(name, value)| format!("({} {})", name, inline_expr(value)))
                .collect();
            format!("(record {})", rendered.join(" "))
        }
        Expr::Update { record, fields } => {
            let rendered: Vec<String> = fields
                .iter()
                .map(|(name, value)| format!("({} {})", name, inline_expr(value)))
                .collect();
            format!("(update {} {})", inline_expr(record), rendered.join(" "))
        }
        Expr::Access { record, field } => {
            format!("(access {} {})", inline_expr(record), field)
        }
        Expr::GetField { of, step } => {
            format!("(get {} {})", inline_expr(of), print_step(*step))
        }
        Expr::TailCall { args } => format!("(tail-call {})", inline_list(args)),
        Expr::Crash(message) => format!("(crash {:?})", message),
        // Compound forms inside an inline position: render on one line.
        Expr::Let { .. }
        | Expr::LetRec { .. }
        | Expr::If { .. }
        | Expr::Case { .. } => {
            let mut nested = String::new();
            print_expr(&mut nested, expr, 0);
            format!(
                "({})",
                nested
                    .lines()
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
    }
}
