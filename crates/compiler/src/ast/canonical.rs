//! Port of `AST.Canonical` — the AST after name resolution.
//!
//! Variables are resolved to their homes (local, top-level, foreign module,
//! or constructor), binop chains are rebuilt into trees using precedence
//! and associativity, and type aliases are expanded away.

use crate::data::Name;
use crate::reporting::{Located, Region};
use std::rc::Rc;

pub type Expr = Located<Expr_>;

#[derive(Debug, Clone)]
pub enum Expr_ {
    VarLocal(Name),
    VarTopLevel(Name),
    /// A value from a built-in module, e.g. `VarForeign("List", "map")`.
    VarForeign(Name, Name),
    /// A constructor reference: (union home module, union name, ctor).
    VarCtor(Name, Name, Ctor),
    Chr(char),
    Str(String),
    Int(i64),
    Float(f64),
    List(Vec<Expr>),
    Negate(Box<Expr>),
    /// op, home module, function name, left, right.
    Binop(Name, Name, Name, Box<Expr>, Box<Expr>),
    Lambda(Vec<Pattern>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    If(Vec<(Expr, Expr)>, Box<Expr>),
    /// Sequential, dependency-ordered declarations. Mutually recursive
    /// groups are only legal if every member is a function.
    Let(Vec<LetDecl>, Box<Expr>),
    Case(Box<Expr>, Vec<(Pattern, Expr)>),
    Accessor(Name),
    Access(Box<Expr>, Located<Name>),
    Update(Box<Expr>, Vec<(Located<Name>, Expr)>),
    Record(Vec<(Located<Name>, Expr)>),
    Unit,
    Tuple(Box<Expr>, Box<Expr>, Vec<Expr>),
    /// A `[glsl| ... |]` WebGL shader literal.
    Shader(crate::ast::source::Shader),
}

#[derive(Debug, Clone)]
pub enum LetDecl {
    /// A definition that does not reference itself.
    Def(Def),
    /// One or more definitions that reference each other; all functions.
    Recursive(Vec<Def>),
    Destruct(Pattern, Expr),
}

#[derive(Debug, Clone)]
pub struct Def {
    pub name: Located<Name>,
    pub args: Vec<Pattern>,
    pub body: Expr,
    pub annotation: Option<Type>,
}

/// Everything the compiler needs to know about a constructor occurrence.
#[derive(Debug, Clone)]
pub struct Ctor {
    pub name: Name,
    pub index: u32,
    pub arity: u32,
    /// Number of constructors in the union — lets codegen skip tag tests
    /// when a union has a single constructor.
    pub num_ctors: u32,
}

pub type Pattern = Located<Pattern_>;

#[derive(Debug, Clone)]
pub enum Pattern_ {
    Anything,
    Var(Name),
    Record(Vec<Located<Name>>),
    Alias(Box<Pattern>, Located<Name>),
    Unit,
    Tuple(Box<Pattern>, Box<Pattern>, Vec<Pattern>),
    /// home module, union name, ctor info, argument patterns.
    Ctor(Name, Name, Ctor, Vec<Pattern>),
    List(Vec<Pattern>),
    Cons(Box<Pattern>, Box<Pattern>),
    Chr(char),
    Str(String),
    Int(i64),
}

// TYPES

/// The recursive positions are `Rc`-shared so that cloning a `Type` is O(1)
/// (a refcount bump) and structurally-identical subtrees are physically shared
/// rather than duplicated. Deeply-nested type aliases (records within records)
/// otherwise expand into a fully-duplicated tree — a small type with a few
/// hundred distinct subtrees can balloon into hundreds of thousands of nodes
/// (elm-athlete's `computeBlock`), exhausting memory during specialization.
/// `Rc` keeps the DAG shared. Structural `PartialEq`/`Eq`/`Hash` are preserved
/// (they delegate through the `Rc`), so types remain usable as map keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Var(Name),
    Lambda(Rc<Type>, Rc<Type>),
    /// home module, type name, arguments. Aliases are already expanded.
    Type(Name, Name, Rc<Vec<Type>>),
    Record(Rc<Vec<(Name, Type)>>, Option<Name>),
    Unit,
    Tuple(Rc<Type>, Rc<Type>, Option<Rc<Type>>),
}

// MODULE

#[derive(Debug, Clone)]
pub struct Module {
    pub name: Name,
    /// Top-level definitions sorted into dependency order. Each group is
    /// either a single definition or a set of mutually recursive functions.
    pub decls: Vec<DeclGroup>,
    pub unions: Vec<Union>,
    pub ports: Vec<PortDecl>,
}

#[derive(Debug, Clone)]
pub struct PortDecl {
    pub name: Name,
    pub tipe: Type,
}

#[derive(Debug, Clone)]
pub enum DeclGroup {
    /// Definition that does not reference itself.
    Value(Def),
    /// One or more definitions that reference each other (or themselves).
    Recursive(Vec<Def>),
}

#[derive(Debug, Clone)]
pub struct Union {
    pub name: Name,
    pub vars: Vec<Name>,
    pub ctors: Vec<UnionCtor>,
}

#[derive(Debug, Clone)]
pub struct UnionCtor {
    pub name: Name,
    pub index: u32,
    pub args: Vec<Type>,
    pub region: Region,
}
