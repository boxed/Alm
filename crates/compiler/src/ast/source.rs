//! Port of `AST.Source` — the AST produced by the parser, before
//! canonicalization. Shapes mirror the Haskell definitions; GLSL shaders
//! and effect managers are not ported yet.

use crate::data::Name;
use crate::reporting::{Located, Region};

// EXPRESSIONS

pub type Expr = Located<Expr_>;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr_ {
    Chr(char),
    Str(String),
    Int(i64),
    Float(f64),
    Var(VarType, Name),
    VarQual(VarType, Name, Name),
    List(Vec<Expr>),
    Op(Name),
    Negate(Box<Expr>),
    /// A flat chain `e1 op1 e2 op2 ... opN eN`; precedence is resolved
    /// during canonicalization, exactly like the Haskell compiler.
    Binops(Vec<(Expr, Located<Name>)>, Box<Expr>),
    Lambda(Vec<Pattern>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    If(Vec<(Expr, Expr)>, Box<Expr>),
    Let(Vec<Located<Def>>, Box<Expr>),
    Case(Box<Expr>, Vec<(Pattern, Expr)>),
    Accessor(Name),
    Access(Box<Expr>, Located<Name>),
    Update(Located<Name>, Vec<(Located<Name>, Expr)>),
    Record(Vec<(Located<Name>, Expr)>),
    Unit,
    Tuple(Box<Expr>, Box<Expr>, Vec<Expr>),
    /// A `[glsl| ... |]` WebGL shader literal.
    Shader(Shader),
}

/// A GLSL shader literal: its source plus the `attribute`/`uniform`/`varying`
/// names declared in it (used to synthesize the `WebGL.Shader` record types
/// and to emit the shader object during code generation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shader {
    pub src: String,
    pub attributes: Vec<Name>,
    pub uniforms: Vec<Name>,
    pub varyings: Vec<Name>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarType {
    LowVar,
    CapVar,
}

// DEFINITIONS

#[derive(Debug, Clone, PartialEq)]
pub enum Def {
    Define(Located<Name>, Vec<Pattern>, Expr, Option<Type>),
    Destruct(Pattern, Expr),
}

// PATTERNS

pub type Pattern = Located<Pattern_>;

#[derive(Debug, Clone, PartialEq)]
pub enum Pattern_ {
    Anything,
    Var(Name),
    Record(Vec<Located<Name>>),
    Alias(Box<Pattern>, Located<Name>),
    Unit,
    Tuple(Box<Pattern>, Box<Pattern>, Vec<Pattern>),
    Ctor(Region, Name, Vec<Pattern>),
    CtorQual(Region, Name, Name, Vec<Pattern>),
    List(Vec<Pattern>),
    Cons(Box<Pattern>, Box<Pattern>),
    Chr(char),
    Str(String),
    Int(i64),
}

// TYPES

pub type Type = Located<Type_>;

#[derive(Debug, Clone, PartialEq)]
pub enum Type_ {
    Lambda(Box<Type>, Box<Type>),
    Var(Name),
    Type(Region, Name, Vec<Type>),
    TypeQual(Region, Name, Name, Vec<Type>),
    Record(Vec<(Located<Name>, Type)>, Option<Located<Name>>),
    Unit,
    Tuple(Box<Type>, Box<Type>, Vec<Type>),
}

// MODULE

#[derive(Debug, Clone)]
pub struct Module {
    pub name: Option<Located<Name>>,
    pub exports: Located<Exposing>,
    pub imports: Vec<Import>,
    pub values: Vec<Located<Value>>,
    pub unions: Vec<Located<Union>>,
    pub aliases: Vec<Located<Alias>>,
    pub binops: Vec<Located<Infix>>,
    pub ports: Vec<Port>,
}

#[derive(Debug, Clone)]
pub struct Port {
    pub name: Located<Name>,
    pub tipe: Type,
}

impl Module {
    pub fn get_name(&self) -> Name {
        match &self.name {
            Some(located) => located.value.clone(),
            None => Name::from_str("Main"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Import {
    pub name: Located<Name>,
    pub alias: Option<Name>,
    pub exposing: Exposing,
}

#[derive(Debug, Clone)]
pub struct Value {
    pub name: Located<Name>,
    pub args: Vec<Pattern>,
    pub body: Expr,
    pub type_annotation: Option<Type>,
}

#[derive(Debug, Clone)]
pub struct Union {
    pub name: Located<Name>,
    pub vars: Vec<Located<Name>>,
    pub ctors: Vec<(Located<Name>, Vec<Type>)>,
}

#[derive(Debug, Clone)]
pub struct Alias {
    pub name: Located<Name>,
    pub vars: Vec<Located<Name>>,
    pub tipe: Type,
}

#[derive(Debug, Clone)]
pub struct Infix {
    pub op: Name,
    pub associativity: Associativity,
    pub precedence: u8,
    pub function: Name,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Associativity {
    Left,
    Non,
    Right,
}

// EXPOSING

#[derive(Debug, Clone)]
pub enum Exposing {
    Open,
    Explicit(Vec<Exposed>),
}

#[derive(Debug, Clone)]
pub enum Exposed {
    Lower(Located<Name>),
    Upper(Located<Name>, Privacy),
    Operator(Region, Name),
}

#[derive(Debug, Clone)]
pub enum Privacy {
    Public(Region),
    Private,
}
