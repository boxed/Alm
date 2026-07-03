//! Port of `AST.Canonical` — the AST after name resolution.
//!
//! Variables are resolved to their homes (local, top-level, foreign module,
//! or constructor), binop chains are rebuilt into trees using precedence
//! and associativity, and type aliases are expanded away.

use crate::data::Name;
use crate::reporting::{Located, Region};

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

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Var(Name),
    Lambda(Box<Type>, Box<Type>),
    /// home module, type name, arguments. Aliases are already expanded.
    Type(Name, Name, Vec<Type>),
    Record(Vec<(Name, Type)>, Option<Name>),
    Unit,
    Tuple(Box<Type>, Box<Type>, Option<Box<Type>>),
}

impl Type {
    pub fn int() -> Type {
        Type::Type(Name::from("Basics"), Name::from("Int"), vec![])
    }
    pub fn float() -> Type {
        Type::Type(Name::from("Basics"), Name::from("Float"), vec![])
    }
    pub fn string() -> Type {
        Type::Type(Name::from("String"), Name::from("String"), vec![])
    }
    pub fn char() -> Type {
        Type::Type(Name::from("Char"), Name::from("Char"), vec![])
    }
    pub fn bool() -> Type {
        Type::Type(Name::from("Basics"), Name::from("Bool"), vec![])
    }
    pub fn list(item: Type) -> Type {
        Type::Type(Name::from("List"), Name::from("List"), vec![item])
    }
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
